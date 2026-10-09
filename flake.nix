{
  description = "Pick and clean up Git worktrees across projects";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          gitYard =
            pkgs:
            pkgs.rustPlatform.buildRustPackage {
              pname = "git-yard";
              version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
              src = self;
              cargoLock.lockFile = ./Cargo.lock;
              nativeBuildInputs = [
                pkgs.buildPackages.pkg-config
                pkgs.buildPackages.installShellFiles
              ];
              buildInputs = [ pkgs.libgit2 ];
              # Link against the Nix libgit2 instead of the crate's vendored copy.
              env.LIBGIT2_NO_VENDOR = "1";
              # PCRE2's ARM64 JIT needs __clear_cache; Rust omits GCC's runtime.
              env.RUSTFLAGS = nixpkgs.lib.optionalString (
                pkgs.stdenv.hostPlatform.isStatic && pkgs.stdenv.hostPlatform.isAarch64
              ) "-C link-arg=-lgcc";
              nativeCheckInputs = [ pkgs.buildPackages.git ];
              postInstall = nixpkgs.lib.optionalString (pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform) ''
                installShellCompletion --cmd git-yard \
                  --bash <($out/bin/git-yard completions bash) \
                  --zsh <($out/bin/git-yard completions zsh) \
                  --fish <($out/bin/git-yard completions fish)
              '';
              meta.mainProgram = "git-yard";
            };
        in
        {
          default = gitYard pkgs;
        }
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # Fully static musl binary that runs on any Linux of the same
          # architecture, without Nix.
          static = gitYard pkgs.pkgsStatic;
        }
      );

      homeManagerModules.default = import ./nix/home-manager.nix self;
      homeManagerModules.git-yard = self.homeManagerModules.default;

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
          packages = [
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
          ];
          env.LIBGIT2_NO_VENDOR = "1";
        };
      });
    };
}
