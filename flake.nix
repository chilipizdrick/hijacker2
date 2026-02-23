{
  description = "hijacker2 nix flake";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs = inputs:
    inputs.flake-parts.lib.mkFlake {inherit inputs;} {
      systems = ["x86_64-linux"];
      perSystem = {
        pkgs,
        lib,
        ...
      }: {
        devShells.default = pkgs.mkShell rec {
          buildInputs = with pkgs; [
            pkg-config

            clang-tools
            llvmPackages.libstdcxxClang

            pipewire
          ];
          LD_LIBRARY_PATH = "${pkgs.lib.makeLibraryPath buildInputs}";
          CPATH = lib.makeSearchPathOutput "dev" "include" buildInputs;
        };
      };
    };
}
