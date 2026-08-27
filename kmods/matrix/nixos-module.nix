{ nixpkgsPath, moduleSource }:

let
  pkgs = import nixpkgsPath { };
  kernel = pkgs.linuxPackages.kernel;
in
pkgs.stdenv.mkDerivation {
  pname = "zcnblk-client-module";
  version = kernel.modDirVersion;
  src = moduleSource;

  nativeBuildInputs = kernel.moduleBuildDependencies ++ [ pkgs.kmod ];
  makeFlags = [
    "KDIR=${kernel.dev}/lib/modules/${kernel.modDirVersion}/build"
  ];

  installPhase = ''
    runHook preInstall
    mkdir -p "$out"
    install -m 0644 zcnblk_client_mod.ko "$out/zcnblk_client_mod.ko"
    ${pkgs.kmod}/bin/modinfo -F name "$out/zcnblk_client_mod.ko" > "$out/module-name.txt"
    ${pkgs.kmod}/bin/modinfo -F vermagic "$out/zcnblk_client_mod.ko" > "$out/vermagic.txt"
    runHook postInstall
  '';
}
