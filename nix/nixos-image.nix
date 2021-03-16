# $ nix-build minimal-vm.nix
# $ install -m600 ./result/nixos.qcow2 ../../linux/nixos.qcow2
# $ ./run-vm.sh
{ pkgs ? (import (import ../nix/sources.nix).nixpkgs { }) }:

let
  keys = map (key: "${builtins.getEnv "HOME"}/.ssh/${key}")
    ["id_rsa.pub" "id_ecdsa.pub" "id_ed25519.pub"];
in import (pkgs.path + "/nixos/lib/make-disk-image.nix") {
  inherit pkgs;
  inherit (pkgs) lib;
  config = (import (pkgs.path + "/nixos/lib/eval-config.nix") {
    inherit (pkgs) system;
    modules = [({ lib, ... }: {
      imports = [
        (pkgs.path + "/nixos/modules/profiles/qemu-guest.nix")
      ];
      boot.loader.grub.enable = false;
      boot.initrd.enable = false;
      boot.isContainer = true;
      boot.loader.initScript.enable = true;
      # login with empty password
      users.extraUsers.root.initialHashedPassword = "";
      services.openssh.enable = true;

      users.users.root.openssh.authorizedKeys.keyFiles = lib.filter builtins.pathExists keys;
      networking.firewall.enable = false;

      services.getty.helpLine = ''
        Log in as "root" with an empty password.
        If you are connect via serial console:
        Type Ctrl-a c to switch to the qemu console
        and `quit` to stop the VM.
      '';
      documentation.doc.enable = false;
      environment.systemPackages = [ pkgs.kmod pkgs.qemu pkgs.gcc pkgs.linuxPackages.bcc ];
    })];
  }).config;
  partitionTableType = "none";
  diskSize = 8192;
  format = "qcow2";
}
