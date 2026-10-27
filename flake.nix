{
  description = "workforce (wf): coordinates coding agents working on a git repository across machines";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      overlays = [
        rust-overlay.overlays.default
        (final: prev: {
          rustToolchain = final.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        })
      ];

      supportedSystems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forEachSupportedSystem = makeOutputs: nixpkgs.lib.genAttrs supportedSystems (system: makeOutputs {
        pkgs = import nixpkgs { inherit overlays system; };
      });
    in
    {
      packages = forEachSupportedSystem ({ pkgs }:
        let
          rustPlatform = pkgs.makeRustPlatform {
            cargo = pkgs.rustToolchain;
            rustc = pkgs.rustToolchain;
          };
        in
        {
          default = rustPlatform.buildRustPackage {
            pname = "wf";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
            src = pkgs.lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "--package" "wf-cli" ];

            # The tests need git signing, the network and Fence, which the build sandbox has not:
            # `just check` runs them.
            doCheck = false;

            nativeBuildInputs = [ pkgs.makeWrapper ];
            # wf runs git, and agents run in Fence; the ones on the PATH come first.
            postInstall = ''
              wrapProgram $out/bin/wf --suffix PATH : ${pkgs.lib.makeBinPath [ pkgs.git pkgs.fence ]}
            '';

            meta = {
              description = "Coordinates coding agents working on a git repository across machines";
              license = pkgs.lib.licenses.gpl3Only;
              mainProgram = "wf";
            };
          };
        });

      devShells = forEachSupportedSystem ({ pkgs }: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain
            rust-analyzer
            cargo-edit
            cargo-outdated
            just
            git
            fence
            nixpkgs-fmt
            jq
          ];

          env = {
            RUST_BACKTRACE = "1";
            RUST_SRC_PATH = "${pkgs.rustToolchain}/lib/rustlib/src/rust/library";
          };
        };
      });

      formatter = forEachSupportedSystem ({ pkgs }: pkgs.nixpkgs-fmt);
    };
}
