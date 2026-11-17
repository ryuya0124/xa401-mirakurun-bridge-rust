[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$settings = Get-Content -Raw -LiteralPath (Join-Path $repo 'openssl-build.conf') | ConvertFrom-StringData
$version = $settings.OPENSSL_VERSION
$expected = $settings.OPENSSL_SHA256
$base = Join-Path $repo "target/openssl-$version-msvc-utf8"
$prefix = Join-Path $base 'install'
$archive = Join-Path $base "openssl-$version.tar.gz"
$stamp = Join-Path $prefix '.xa401-source-sha256'
$env:LC_ALL = 'C'
$env:LC_CTYPE = 'C'
$env:LANG = 'C'
$env:CL = (($env:CL, '/utf-8') -join ' ').Trim()
if (-not $env:PERL) { $env:PERL = 'C:\Strawberry\perl\bin\perl.exe' }
if (-not (Test-Path -LiteralPath $env:PERL)) { throw 'Native Strawberry Perl is required' }
if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
    $vsPath = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($LASTEXITCODE -ne 0 -or -not $vsPath) { throw 'MSVC x64 build tools are required' }
    Import-Module (Join-Path $vsPath 'Common7/Tools/Microsoft.VisualStudio.DevShell.dll')
    Enter-VsDevShell -VsInstallPath $vsPath -Arch amd64 -HostArch amd64 -SkipAutomaticLocation | Out-Null
}
New-Item -ItemType Directory -Path $base -Force | Out-Null
if (-not (Test-Path -LiteralPath $archive)) {
    Invoke-WebRequest -Uri "https://github.com/openssl/openssl/releases/download/openssl-$version/openssl-$version.tar.gz" -OutFile "$archive.part"
    Move-Item -LiteralPath "$archive.part" -Destination $archive
}
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $expected) { throw 'OpenSSL checksum mismatch' }
if (-not (Test-Path -LiteralPath $stamp) -or (Get-Content -Raw -LiteralPath $stamp).Trim() -ne $expected) {
    & tar -xf $archive -C $base
    if ($LASTEXITCODE -ne 0) { throw 'OpenSSL extraction failed' }
    Push-Location (Join-Path $base "openssl-$version")
    try {
        $installPath = $prefix.Replace('\', '/')
        & $env:PERL ./Configure VC-WIN64A no-shared no-module no-tests no-comp no-zlib no-asm no-static-vcruntime "--prefix=$installPath" --libdir=lib
        if ($LASTEXITCODE -ne 0) { throw 'OpenSSL configuration failed' }
        & nmake /nologo build_libs
        if ($LASTEXITCODE -ne 0) { throw 'OpenSSL compilation failed' }
        & nmake /nologo install_dev
        if ($LASTEXITCODE -ne 0) { throw 'OpenSSL installation failed' }
        Set-Content -LiteralPath $stamp -Value $expected -Encoding ascii
    } finally { Pop-Location }
}
$env:OPENSSL_DIR = $prefix
$env:OPENSSL_STATIC = '1'
Write-Output "OpenSSL $version ready: $prefix"
