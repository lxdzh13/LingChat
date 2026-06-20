@echo off
cd /d C:\Users\DCY45\Desktop\LingChat\1
set JAVA_HOME=C:\Program Files\Java\jdk-21.0.10
set ANDROID_HOME=%LOCALAPPDATA%\Android\Sdk
set ANDROID_SDK_ROOT=%LOCALAPPDATA%\Android\Sdk
set NDK_HOME=%LOCALAPPDATA%\Android\Sdk\ndk\28.2.13676358
set PATH=C:\Program Files\Java\jdk-21.0.10\bin;%LOCALAPPDATA%\Android\Sdk\platform-tools;%LOCALAPPDATA%\Android\Sdk\build-tools\35.0.0;%USERPROFILE%\.cargo\bin;%PATH%
call node_modules\.bin\tauri.cmd android build --apk
echo TauriBuildExitCode=%ERRORLEVEL% > .tauri-build-done.flag

