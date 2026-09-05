{
  # Dev toolchain for pall8t, pinned by flake.lock. This replaces mise as the
  # version manager: the same flake provisions the host shell (`nix develop`)
  # and the sandbox image (.pall8t/Containerfile COPYs flake.nix + flake.lock
  # and installs #rust-toolchain from them), so both sides build with the
  # exact toolchain flake.lock names.
  #
  # Note: apple/container itself is a system service (installer pkg +
  # launchd), not manageable here — install it from
  # https://github.com/apple/container/releases
  description = "pall8t dev toolchain (Rust, pinned by flake.lock)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      # The sandbox VM is aarch64-linux (apple/container on Apple silicon);
      # aarch64-darwin covers developing on the Mac host directly, and
      # x86_64-linux other Linux dev machines. No x86_64-darwin: the pinned
      # nixpkgs (26.11) dropped that platform — every eval throws — and
      # apple/container needs Apple silicon, so pall8t can't run there
      # anyway.
      systems = [ "aarch64-linux" "x86_64-linux" "aarch64-darwin" ];
      forEach = f:
        nixpkgs.lib.genAttrs systems (system:
          f (import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          }));
    in {
      packages = forEach (pkgs: rec {
        # Matches Cargo.toml's rust-version (the MSRV clippy enforces).
        # rust-analyzer + rust-src on top of the default profile for editor
        # tooling; the aarch64-apple-darwin std backs scripts/lint.sh's
        # cross-target clippy gate on cfg(target_os = "macos") code, which
        # would otherwise be invisible when linting from Linux.
        rust-toolchain = pkgs.rust-bin.stable."1.96.0".default.override {
          extensions = [ "rust-analyzer" "rust-src" ];
          targets = [ "aarch64-apple-darwin" ];
        };

        # Userland tools for the sandbox image: .pall8t/Containerfile builds
        # this into /usr/local/tools, so the whole set is pinned by
        # flake.lock instead of floating with apt/NodeSource/GitHub-CLI
        # repository state at image-build time. node feeds the npm install
        # of the claude CLI (which stays npm so the image gets the current
        # CLI, not a nixpkgs snapshot). What the image deliberately keeps on
        # apt instead: bootstrap (ca-certificates/curl fetch nix itself),
        # sudo (setuid — a nix-store binary can't be), openssh-client (owns
        # /etc/ssh, where GitHub's host keys are baked), and the C link
        # chain cc/pkg-config/mold (linking stays on the distro toolchain —
        # see the Containerfile).
        sandbox-tools = pkgs.buildEnv {
          name = "pall8t-sandbox-tools";
          paths = with pkgs; [ git ripgrep jq less vim gh nodejs_22 ];
        };

        default = rust-toolchain;
      });

      devShells = forEach (pkgs: {
        default = pkgs.mkShell {
          packages = [ self.packages.${pkgs.stdenv.hostPlatform.system}.rust-toolchain ];
        };
      });
    };
}
