{
  description = "Scarlet Nintendo Switch T210 BSP and L4T bring-up tools";

  nixConfig = {
    extra-substituters = [ "https://scarlet-rust-toolchain.cachix.org" ];
    extra-trusted-public-keys = [ "scarlet-rust-toolchain.cachix.org-1:p+coBExi0nNTIvWF/oM9H9/1/GhwFtqGZ2Vs+4pYl6o=" ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    scarlet-rust-toolchain.url = "github:petitstrawberry/scarlet-rust-nix";
    scarlet-sdk = {
      url = "github:petitstrawberry/scarlet-sdk/116882ab42b48a23613d3d574e2c987aca277786";
      flake = false;
    };
    rust-overlay.follows = "scarlet-rust-toolchain/rust-overlay";
    switchvisor-src = {
      url = "github:petitstrawberry/switchvisor/8a84a0be7d1aebe22a6636b80319abb37edfef8a";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, scarlet-rust-toolchain, scarlet-sdk, rust-overlay, switchvisor-src, ... }:
    let
      systems = [ "aarch64-darwin" "aarch64-linux" "x86_64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      mkNxboot = pkgs: pkgs.runCommand "nxboot-0.3.2" { } ''
        mkdir -p "$out/bin"
        cp ${pkgs.fetchurl {
          url = "https://github.com/mologie/nxboot/releases/download/v0.3.2/nxboot";
          hash = "sha256-29uszEZDZ6vv9uzTeSuQRCsM8XsI4WkLv6kJC01ZVg4=";
        }} "$out/bin/nxboot"
        chmod +x "$out/bin/nxboot"
      '';
    in {
      packages = eachSystem (system:
        let
          pkgs = pkgsFor system;
          switchvisorRust = pkgs.rust-bin.stable.latest.minimal.override {
            targets = [ "aarch64-unknown-none-softfloat" ];
          };
          rustPlatform = pkgs.makeRustPlatform {
            cargo = switchvisorRust;
            rustc = switchvisorRust;
          };
          switchvisor = rustPlatform.buildRustPackage {
            pname = "switchvisor";
            version = "0.1.0-${builtins.substring 0 7 (switchvisor-src.rev or "local")}";
            src = switchvisor-src;
            cargoLock.lockFile = "${switchvisor-src}/Cargo.lock";
            cargoBuildFlags = [ "-p" "switchvisorctl" "-p" "switchvisor-tool" ];
            cargoTestFlags = [ "--workspace" ];
            RUSTDOC = "${switchvisorRust}/bin/rustdoc";
            nativeBuildInputs = [ pkgs.llvmPackages.llvm pkgs.dtc pkgs.python3 ];
            # cargo-auditable adds host-linker flags that rust-lld cannot use for EL2.
            auditable = false;
            postBuild = ''
              env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS \
                cargo build --offline --locked -p switchvisor --bin switchvisor \
                --features baremetal --target aarch64-unknown-none-softfloat --release
              llvm-objcopy -O binary target/aarch64-unknown-none-softfloat/release/switchvisor bootstrap.raw
              for profile in uart control net; do
                dtc -@ -I dts -O dtb -o usb-$profile.dtbo config/tegra210-usb-$profile.dts
              done
            '';
            postCheck = ''
              python3 scripts/check-el2-isa.py target/aarch64-unknown-none-softfloat/release/switchvisor
            '';
            postInstall = ''
              mkdir -p "$out/share/switchvisor"
              install -m644 bootstrap.raw usb-*.dtbo "$out/share/switchvisor/"
              cp ${pkgs.writeText "switchvisor-source.json" (builtins.toJSON {
                git = "https://github.com/petitstrawberry/switchvisor";
                rev = switchvisor-src.rev or null;
              })} "$out/share/switchvisor/source.json"
            '';
            meta = {
              description = "Switchvisor EL2 monitor, USB control CLI and image tools";
              homepage = "https://github.com/petitstrawberry/switchvisor";
              license = pkgs.lib.licenses.gpl2Only;
              platforms = systems;
              mainProgram = "switchvisorctl";
            };
          };
        in {
          inherit switchvisor;
          switchvisorctl = switchvisor;
          switchvisor-tool = switchvisor;
        } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin { nxboot = mkNxboot pkgs; });
      devShells = eachSystem (system:
        let
          pkgs = pkgsFor system;
          rust = scarlet-rust-toolchain.packages.${system}.scarlet-rust-toolchain;
          switchvisor = self.packages.${system}.switchvisor;
          sdk = pkgs.rustPlatform.buildRustPackage {
            pname = "cargo-scarlet";
            version = "1.0.0";
            src = scarlet-sdk;
            buildAndTestSubdir = "cargo-scarlet";
            cargoLock.lockFile = "${scarlet-sdk}/Cargo.lock";
            nativeBuildInputs = [ pkgs.curl ];
          };
        in {
          default = pkgs.mkShell {
            packages = [
              rust sdk switchvisor pkgs.python3 pkgs.ripgrep pkgs.git pkgs.gh pkgs.curl
              pkgs.llvmPackages.llvm pkgs.dtc pkgs.cpio pkgs.qemu
              pkgs.cmake pkgs.e2fsprogs pkgs.minicom
              pkgs.pkgsCross.aarch64-multiplatform.buildPackages.gcc
            ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ (mkNxboot pkgs) ];
            hardeningDisable = [ "zerocallusedregs" ];
            SWITCHVISOR_DATA = "${switchvisor}/share/switchvisor";
            shellHook = ''
              export PATH="${rust}/bin:$PWD/scripts:$PATH"
              export SCARLET_RUST_ACTIVE_BIN="${rust}/bin"
              # Match Scarlet's cross-C default. Per-application CC_<target>
              # settings can still choose a Linux sysroot when needed (yt).
              # Plain Clang also supplies the target flag to CMake builds.
              export TARGET_CC=${pkgs.llvmPackages.clang-unwrapped}/bin/clang
            '';
          };
        });
    };
}
