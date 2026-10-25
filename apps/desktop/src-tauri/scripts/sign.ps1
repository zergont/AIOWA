# Подпись бинарников сборки (вызывает tauri build: bundle > windows > signCommand, "%1" — путь к файлу).
#
# Сертификат задаётся переменными окружения, в репозитории его нет:
#   AIOWA_SIGN_THUMBPRINT    — SHA1-отпечаток сертификата в хранилище «Личное» (CurrentUser\My или
#                              LocalMachine\My, в том числе на токене);
#   AIOWA_SIGN_PFX           — либо путь к .pfx, пароль в AIOWA_SIGN_PFX_PASSWORD;
#   AIOWA_SIGN_TIMESTAMP_URL — сервер меток времени RFC 3161 (по умолчанию http://timestamp.digicert.com);
#   AIOWA_SIGNTOOL           — путь к signtool.exe, если его нет в PATH и в Windows SDK.
# Без сертификата подпись пропускается, сборка не падает: неподписанный установщик работает, но
# SmartScreen показывает «Неизвестный издатель».
param([Parameter(Mandatory = $true)][string]$File)

$ErrorActionPreference = 'Stop'
$name = Split-Path -Leaf $File

$thumb = $env:AIOWA_SIGN_THUMBPRINT
$pfx = $env:AIOWA_SIGN_PFX
if (-not $thumb -and -not $pfx) {
    Write-Host "[sign] подпись пропущена: не заданы AIOWA_SIGN_THUMBPRINT и AIOWA_SIGN_PFX ($name)"
    exit 0
}

function Find-SignTool {
    if ($env:AIOWA_SIGNTOOL) { return $env:AIOWA_SIGNTOOL }
    $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    if (Test-Path $kits) {
        $found = Get-ChildItem -Path $kits -Filter signtool.exe -Recurse -ErrorAction SilentlyContinue |
            Where-Object { $_.DirectoryName -match '\x64$' } |
            Sort-Object { [version]($_.Directory.Parent.Name -replace '[^0-9.]', '') } -Descending |
            Select-Object -First 1
        if ($found) { return $found.FullName }
    }
    return $null
}

$signtool = Find-SignTool
if (-not $signtool) {
    Write-Error "[sign] signtool.exe не найден: установите Windows SDK (Signing Tools) или задайте AIOWA_SIGNTOOL"
    exit 1
}

$timestamp = if ($env:AIOWA_SIGN_TIMESTAMP_URL) { $env:AIOWA_SIGN_TIMESTAMP_URL } else { 'http://timestamp.digicert.com' }
$signArgs = @('sign', '/fd', 'sha256', '/tr', $timestamp, '/td', 'sha256', '/d', 'AIOWA')
if ($thumb) {
    $signArgs += @('/sha1', ($thumb -replace '\s', ''))
} else {
    $signArgs += @('/f', $pfx)
    if ($env:AIOWA_SIGN_PFX_PASSWORD) { $signArgs += @('/p', $env:AIOWA_SIGN_PFX_PASSWORD) }
}
$signArgs += $File

Write-Host "[sign] подписываю $name"
& $signtool @signArgs
if ($LASTEXITCODE -ne 0) {
    Write-Error "[sign] signtool завершился с кодом $LASTEXITCODE ($name)"
    exit $LASTEXITCODE
}
& $signtool verify /pa /q $File
if ($LASTEXITCODE -ne 0) {
    Write-Error "[sign] подпись $name не прошла проверку"
    exit $LASTEXITCODE
}
Write-Host "[sign] готово: $name"
