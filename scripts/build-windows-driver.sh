#!/usr/bin/env bash
# Cross-build the native block edge using the official WDK, SDK and VC headers.
# Signing/catalog generation is a separate Windows packaging step.
set -euo pipefail
cd "$(dirname "$(realpath "$0")")/.."
: "${WDK_ROOT:?Set WDK_ROOT to the extracted Microsoft.Windows.WDK.x64 package}"
: "${SDK_ROOT:?Set SDK_ROOT to the extracted Microsoft.Windows.SDK.CPP package}"
: "${VC_INCLUDE:?Set VC_INCLUDE to Microsoft MSVC include directory}"
sdk_version=${WINDOWS_SDK_VERSION:-10.0.26100.0}
driver_out=${WINDOWS_DRIVER_OUT:-target/windows-driver}
mkdir -p "$driver_out"
driver_out=$(realpath "$driver_out")
km=$(realpath "$WDK_ROOT/c/Include/$sdk_version/km")
shared=$(realpath "$SDK_ROOT/c/Include/$sdk_version/shared")
ucrt=$(realpath "$SDK_ROOT/c/Include/$sdk_version/ucrt")
vc=$(realpath "$VC_INCLUDE")
python3 - "$driver_out/headers.json" "$km" "$shared" "$ucrt" "$vc" <<'PY'
import json, pathlib, sys
def entry(path):
    if path.is_dir():
        return {'type':'directory', 'name':str(path), 'contents':[entry(p) for p in path.iterdir()]}
    return {'type':'file', 'name':path.name, 'external-contents':str(path)}
pathlib.Path(sys.argv[1]).write_text(json.dumps({'version':0, 'case-sensitive':False,
    'roots':[entry(pathlib.Path(p)) for p in sys.argv[2:]]}))
PY
"${CLANG_CL:-clang-cl-19}" /nologo /c /TC /std:c11 /kernel /GS- /Zl /W4 \
  /D_AMD64_ /DAMD64 /D_WIN64 /D_KERNEL_MODE /DNTDDI_VERSION=0x0A000000 /D_WIN32_WINNT=0x0A00 \
  /clang:-ivfsoverlay "/clang:$driver_out/headers.json" \
  "/I$km" "/I$shared" "/I$ucrt" "/I$vc" "/Fo$driver_out/zcnblk.obj" windows/zcnblk/zcnblk.c
# Microsoft kernel libraries carry .retplne metadata. lld must not emit it
# as an unmapped section; keep it resident alongside initialization metadata.
lld-link /nologo /driver /subsystem:native,10.0 /entry:DriverEntry /nodefaultlib \
  /machine:x64 /base:0x140000000 /fixed:no /filealign:4096 /tsaware:no \
  /merge:.retplne=INIT /section:INIT,RWP \
  /section:.text,ERP /section:.rdata,RP /section:.data,RWP /section:.pdata,RP \
  /nxcompat /dynamicbase /integritycheck "/out:$driver_out/zcnblk.sys" \
  "$driver_out/zcnblk.obj" "$WDK_ROOT/c/Lib/$sdk_version/km/x64/ntoskrnl.lib" \
  "$WDK_ROOT/c/Lib/$sdk_version/km/x64/storport.lib"
cp windows/zcnblk/zcnblk.inf "$driver_out/"
printf 'Unsigned native driver: %s/zcnblk.sys\n' "$driver_out"
