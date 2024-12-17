!#/bin/bash
cd /home/yuki/source/repos/wfinfo-ng

./update.sh

cargo run --release --bin wfinfo /mnt/980Pro/SteamLibrary/steamapps/compatdata/230410/pfx/drive_c/users/steamuser/AppData/Local/Warframe/EE.log --window-name=gamescope