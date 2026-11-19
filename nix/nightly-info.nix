# Rolling nightly channel pointer.
#
# THIS FILE IS AUTO-UPDATED by .github/workflows/nightly.yml on the `nightly`
# git ref immediately before the nightly tag is force-moved. On `main` it is
# only a seed: the hash below is a placeholder and the file will NOT fetch
# successfully from main.
#
# Consume nightly via the pinned ref:
#
#   nix run github:0xErwin1/fileflux/nightly#fileflux-nightly
#
# Do NOT pin to main for this package. The `nightly` ref always contains
# hashes that match the artifacts published in the rolling nightly release.
#
# From-source nightly (no hash needed):
#
#   nix run github:0xErwin1/fileflux/nightly#fileflux-source
{
  version = "nightly";

  artifacts = {
    "x86_64-linux" = {
      url = "https://github.com/0xErwin1/fileflux/releases/download/nightly/fileflux-linux-amd64.tar.gz";
      hash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    };
  };
}
