# Prebuilt-binary derivation for Fileflux.
#
# Pulls the matching tarball from the GitHub Release named by the info file
# (./nightly-info.nix, the rolling nightly; its url is a per-build immutable
# asset, not the rolling tarball) and installs it into a Nix store
# path. Avoids compiling from source on the consumer machine.
#
# Supported systems: the ones listed in the info file's `artifacts`
# (x86_64-linux today). Other systems fall back to the source build (see
# flake.nix).

{
  lib,
  stdenv,
  fetchurl,
  autoPatchelfHook,
  makeWrapper,
  # Runtime libraries: must mirror the `buildInputs` of the source build in
  # flake.nix so the patched binary finds the same .so files at runtime.
  openssl,
  zstd,
  zlib,
  fontconfig,
  freetype,
  wayland,
  libxkbcommon,
  libx11,
  libxcb,
  libxcursor,
  libxrandr,
  libxi,
  vulkan-loader,
  alsa-lib,
  libarchive,
  ffmpegthumbnailer,
  stdenv_cc_libs ? stdenv.cc.cc.lib,
  # Which info file to read. Fileflux has no stable release channel yet, so
  # this is the rolling nightly.
  infoFile ? ./nightly-info.nix,
}:

let
  releaseInfo = import infoFile;
  system = stdenv.hostPlatform.system;
  artifact =
    releaseInfo.artifacts.${system} or (throw (
      "fileflux: no prebuilt binary published for system '${system}'. "
      + "Use the source build (packages.fileflux-source) instead."
    ));

  # Per-channel install identity: nightly installs under distinct binary and
  # desktop names so a stable and a nightly package can coexist in one
  # profile (e.g. a home-manager `buildEnv`) without colliding on
  # `bin/fileflux`.
  isNightly = lib.hasInfix "nightly" releaseInfo.version;
  appId = if isNightly then "fileflux-nightly" else "fileflux";
  appName = if isNightly then "Fileflux Nightly" else "Fileflux";

  # The channel suffix of everything that would otherwise be the source
  # package's: the helper's directory, the policy files and the action ids.
  # Fileflux derives the same channel from the launcher's file name
  # (`fileflux-nightly` -> `nightly`, see `channel_of` in src/core/admin.rs)
  # to find its helper (`../libexec/<channel>/`) and its policy file. A
  # non-nightly info file keeps the unsuffixed names, as it already owns
  # `bin/fileflux`.
  channel = if isNightly then "nightly" else null;
  helperDir = if channel == null then "bin" else "libexec/${channel}";
  idSuffix = if channel == null then "" else "-${channel}";
in
stdenv.mkDerivation {
  pname = appId;
  version = releaseInfo.version;

  src = fetchurl {
    inherit (artifact) url hash;
  };

  # The tarball expands directly into the working directory (no leading
  # top-level folder), so unpack manually.
  sourceRoot = ".";
  unpackPhase = ''
    runHook preUnpack
    mkdir -p source
    tar -xzf $src -C source
    runHook postUnpack
  '';

  nativeBuildInputs = [
    autoPatchelfHook
    makeWrapper
  ];

  # Libraries the binary links against (autoPatchelfHook resolves these via
  # DT_NEEDED) plus the ones it dlopens at runtime (added to LD_LIBRARY_PATH
  # by the wrapper below).
  buildInputs = [
    stdenv_cc_libs
    openssl
    zstd
    zlib
    fontconfig
    freetype
    wayland
    libxkbcommon
    libx11
    libxcb
    libxcursor
    libxrandr
    libxi
    vulkan-loader
    alsa-lib
  ];

  dontBuild = true;
  dontStrip = true;

  # Tarball layout, from .github/workflows/nightly.yml:
  #   bin/fileflux
  #   bin/fileflux-owner-helper (optional; installed as
  #     libexec/<channel>/fileflux-owner-helper for a channel)
  #   packaging/dev.fileflux.{owner-helper,open-as-admin}.policy (for distro
  #     packagers; this derivation substitutes the copies in the repository
  #     checkout it was evaluated from, so older tarballs still install)
  #   share/applications/fileflux.desktop
  #   share/icons/hicolor/{scalable,16x16,...,256x256}/apps/fileflux.{svg,png}
  #   LICENSE
  installPhase = ''
    runHook preInstall

    cd source

    install -Dm755 bin/fileflux $out/bin/${appId}

    # Channel-scoped desktop entry and icons, so a stable and a nightly package can
    # coexist in one profile. StartupWMClass stays `fileflux`: it is the
    # window's app id, which the nightly binary does not change.
    install -Dm644 share/applications/fileflux.desktop \
      $out/share/applications/${appId}.desktop
    sed -i \
      -e 's|^Exec=fileflux|Exec=${appId}|' \
      -e 's|^Name=Fileflux$|Name=${appName}|' \
      -e 's|^Icon=fileflux$|Icon=${appId}|' \
      $out/share/applications/${appId}.desktop

    install -Dm644 share/icons/hicolor/scalable/apps/fileflux.svg \
      $out/share/icons/hicolor/scalable/apps/${appId}.svg
    for size in 16 32 48 64 128 256; do
      install -Dm644 "share/icons/hicolor/''${size}x''${size}/apps/fileflux.png" \
        "$out/share/icons/hicolor/''${size}x''${size}/apps/${appId}.png"
    done

    # The root owner helper and its polkit action, when the tarball has the
    # helper (older nightlies do not; Fileflux then falls back to
    # `pkexec chown`). A channel package keeps it under
    # libexec/<channel>/, which is where Fileflux looks for it, so it never
    # shares `bin/fileflux-owner-helper` or a policy file with the source
    # package; the action id carries the channel and its exec.path names
    # the store path.
    if [ -e bin/fileflux-owner-helper ]; then
      install -Dm755 bin/fileflux-owner-helper \
        $out/${helperDir}/fileflux-owner-helper
      mkdir -p $out/share/polkit-1/actions
      substitute ${../packaging/dev.fileflux.owner-helper.policy} \
        $out/share/polkit-1/actions/dev.fileflux.owner-helper${idSuffix}.policy \
        --replace-fail 'id="dev.fileflux.change-owner"' \
                       'id="dev.fileflux.change-owner${idSuffix}"' \
        --replace-fail /usr/bin/fileflux-owner-helper \
                       $out/${helperDir}/fileflux-owner-helper
    fi

    # The "Open as Administrator" polkit action names the installed launcher.
    mkdir -p $out/share/polkit-1/actions
    substitute ${../packaging/dev.fileflux.open-as-admin.policy} \
      $out/share/polkit-1/actions/dev.fileflux.open-as-admin${idSuffix}.policy \
      --replace-fail 'id="dev.fileflux.open-as-admin"' \
                     'id="dev.fileflux.open-as-admin${idSuffix}"' \
      --replace-fail /usr/bin/fileflux $out/bin/${appId}

    install -Dm644 LICENSE $out/share/licenses/${appId}/LICENSE

    runHook postInstall
  '';

  # Wayland/Vulkan/X11 libraries are dlopened at runtime; autoPatchelf cannot
  # detect these because they are not in DT_NEEDED. Inject them via
  # LD_LIBRARY_PATH on the wrapper, plus the host graphics drivers like the
  # source build.
  postFixup = ''
    wrapProgram $out/bin/${appId} \
      --prefix LD_LIBRARY_PATH : "${
        lib.makeLibraryPath [
          wayland
          libxkbcommon
          vulkan-loader
          libx11
          libxcb
          libxcursor
          libxrandr
          libxi
          fontconfig
        ]
      }" \
      --suffix LD_LIBRARY_PATH : "/run/opengl-driver/lib" \
      --suffix PATH : "${lib.makeBinPath [ libarchive ]}" \
      --run 'export XDG_DATA_DIRS="''${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"' \
      --suffix XDG_DATA_DIRS : "${ffmpegthumbnailer}/share"
  '';

  meta = with lib; {
    description = "A file explorer in the Bolt family (prebuilt binary)";
    homepage = "https://github.com/0xErwin1/fileflux";
    license = licenses.asl20;
    mainProgram = appId;
    platforms = builtins.attrNames releaseInfo.artifacts;
    sourceProvenance = with sourceTypes; [ binaryNativeCode ];
  };
}
