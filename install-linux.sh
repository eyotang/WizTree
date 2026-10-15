#!/bin/sh
set -eu
package_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
app_dir="$data_home/WizTree"
mkdir -p "$app_dir" "$data_home/applications"
cp "$package_dir/wiztree" "$app_dir/wiztree"
cp "$package_dir/disk-scan" "$app_dir/disk-scan"
cp "$package_dir/WizTree.png" "$app_dir/WizTree.png"
chmod 755 "$app_dir/wiztree" "$app_dir/disk-scan"
cp "$package_dir/LICENSE" "$package_dir/THIRD_PARTY_NOTICES.md" "$app_dir/"
cp -R "$package_dir/THIRD_PARTY_LICENSES" "$package_dir/docs" "$app_dir/"
# Desktop entries use quoted paths; escape backslashes and quotes, and literal %.
escaped_dir=$(printf '%s' "$app_dir" | sed 's/\\/\\\\/g; s/"/\\"/g; s/%/%%/g')
cat > "$data_home/applications/WizTree.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=WizTree
Comment=Disk space analyzer
Exec="$escaped_dir/wiztree"
Icon=$app_dir/WizTree.png
Terminal=false
Categories=Utility;System;FileTools;
EOF
printf '%s\n' 'WizTree 已安装，可从应用启动器打开。'
