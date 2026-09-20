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
  };

  outputs = { nixpkgs, scarlet-rust-toolchain, scarlet-sdk, ... }:
    let
      systems = [ "aarch64-darwin" "aarch64-linux" "x86_64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
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
        let pkgs = import nixpkgs { inherit system; };
        in pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin { nxboot = mkNxboot pkgs; });
      devShells = eachSystem (system:
        let
          pkgs = import nixpkgs { inherit system; };
          rust = scarlet-rust-toolchain.packages.${system}.scarlet-rust-toolchain;
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
              rust sdk pkgs.python3 pkgs.ripgrep pkgs.git pkgs.gh pkgs.curl
              pkgs.llvmPackages.llvm pkgs.dtc pkgs.cpio pkgs.qemu
              pkgs.cmake pkgs.e2fsprogs
              pkgs.pkgsCross.aarch64-multiplatform.buildPackages.gcc
            ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ (mkNxboot pkgs) ];
            hardeningDisable = [ "zerocallusedregs" ];
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
