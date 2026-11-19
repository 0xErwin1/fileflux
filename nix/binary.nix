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
  #   share/applications/fileflux.desktop
  #   LICENSE
  # TODO: install the icons once the tarball ships them.
  installPhase = ''
    runHook preInstall

    cd source

    install -Dm755 bin/fileflux $out/bin/${appId}

    # Channel-scoped desktop entry, so a stable and a nightly package can
    # coexist in one profile. StartupWMClass stays `fileflux`: it is the
    # window's app id, which the nightly binary does not change.
    install -Dm644 share/applications/fileflux.desktop \
      $out/share/applications/${appId}.desktop
    sed -i \
      -e 's|^Exec=fileflux|Exec=${appId}|' \
      -e 's|^Name=Fileflux$|Name=${appName}|' \
      $out/share/applications/${appId}.desktop

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
      --suffix LD_LIBRARY_PATH : "/run/opengl-driver/lib"
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
