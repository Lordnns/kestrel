#!/bin/bash
# Double-click in Finder to run the kestrel self-test and save report.txt.
cd "$(dirname "$0")" || exit 1
xattr -dr com.apple.quarantine . 2>/dev/null
{
    ./kestrel selftest -v
    echo
    ./kestrel info
} 2>&1 | tee report.txt
echo
echo "Saved report.txt in $(pwd). Please send it back."
read -n 1 -s -r -p "Press any key to close this window."
