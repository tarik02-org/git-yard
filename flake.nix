{
  description = "Pick and clean up Git worktrees across projects";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  nixConfig = {
    extra-substituters = [ "https://tarik02-git-yard.cachix.org" ];
    extra-trusted-public-keys = [
      "tarik02-git-yard.cachix.org-1:l60zkenz0GrTqa6XWMLzU4M72SG1O/2Y2YY479w435s="
    ];
  };

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
            {
              pkgs,
              portable ? false,
            }:
            pkgs.rustPlatform.buildRustPackage {
              pname = "git-yard";
              version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
              src = self;
              cargoLock.lockFile = ./Cargo.lock;
              nativeBuildInputs = [
                pkgs.buildPackages.pkg-config
                pkgs.buildPackages.installShellFiles
              ];
              buildInputs = nixpkgs.lib.optionals (!portable) [ pkgs.libgit2 ];
              # Portable macOS releases bundle libgit2, zlib and libiconv so only Apple's
              # system libraries remain dynamically linked.
              env.LIBGIT2_NO_VENDOR = if portable then "0" else "1";
              env.LIBZ_SYS_STATIC = if portable then "1" else "0";
              # PCRE2's ARM64 JIT needs __clear_cache; Rust omits GCC's runtime.
              env.RUSTFLAGS = nixpkgs.lib.concatStringsSep " " (
                nixpkgs.lib.optional (
                  pkgs.stdenv.hostPlatform.isStatic && pkgs.stdenv.hostPlatform.isAarch64
                ) "-C link-arg=-lgcc"
                # Search the static archive before Darwin's default dylib paths.
                ++ nixpkgs.lib.optional (
                  portable && pkgs.stdenv.hostPlatform.isDarwin
                ) "-L native=${pkgs.pkgsStatic.libiconv.dev}/lib"
              );
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
          default = gitYard { inherit pkgs; };
          release =
            if pkgs.stdenv.hostPlatform.isLinux then
              gitYard { pkgs = pkgs.pkgsStatic; }
            else
              gitYard {
                inherit pkgs;
                portable = true;
              };
        }
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # Fully static musl binary that runs on any Linux of the same
          # architecture, without Nix.
          static = self.packages.${pkgs.stdenv.hostPlatform.system}.release;
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
