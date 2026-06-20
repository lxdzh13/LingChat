@echo off
cd /d C:\Users\DCY45\Desktop\LingChat\1
set JAVA_HOME=C:\Program Files\Java\jdk-21.0.10
set ANDROID_HOME=%LOCALAPPDATA%\Android\Sdk
set ANDROID_SDK_ROOT=%LOCALAPPDATA%\Android\Sdk
set NDK_HOME=%LOCALAPPDATA%\Android\Sdk\ndk\28.2.13676358
set PATH=C:\Program Files\Java\jdk-21.0.10\bin;%LOCALAPPDATA%\Android\Sdk\platform-tools;%LOCALAPPDATA%\Android\Sdk\build-tools\35.0.0;%USERPROFILE%\.cargo\bin;%PATH%
cd src-tauri\gen\android
call gradlew.bat assembleRelease -x :app:rustBuildArm64Release -x :app:rustBuildArmRelease -x :app:rustBuildX86Release -x :app:rustBuildX86_64Release
echo GradleBuildExitCode=%ERRORLEVEL% > ..\..\..\.gradle-build-done.flag

