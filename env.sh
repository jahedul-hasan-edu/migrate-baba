# Source this in Git Bash before building on Windows (GNU toolchain + WinLibs MinGW).
export PATH="$HOME/.cargo/bin:$(cygpath -u "$LOCALAPPDATA")/Microsoft/WinGet/Packages/BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe/mingw64/bin:$PATH"
