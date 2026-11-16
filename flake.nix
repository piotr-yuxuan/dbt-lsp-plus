# SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
{
  description = "Development shell of dbt-lsp-plus";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/31ecbaca1bdefa45dffc409f0250bd8039ea0010";
  };

  outputs = { nixpkgs, ... }:
    let
      # No C compiler wrapper: it splits a project path that holds a space.
      # Linking uses the cc on PATH.
      shell = pkgs: pkgs.mkShellNoCC {
        packages = [
          pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt pkgs.git pkgs.gnumake
          # For the scripts of the release and their tests, and for the workflows.
          pkgs.python3
          pkgs.actionlint
        ];
      };
    in
    {
      devShells.x86_64-linux.default = shell nixpkgs.legacyPackages.x86_64-linux;
      devShells.aarch64-darwin.default = shell nixpkgs.legacyPackages.aarch64-darwin;
    };
}
