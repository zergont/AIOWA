# Saves the OWA password into Windows Credential Manager through owa-probe.
# Asks for the login and the password interactively; neither is printed or written to logs.
# The login (not the password) is kept in .env.local so that later commands can reuse it.
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$exe = Join-Path $here 'target\debug\owa-probe.exe'
if (-not (Test-Path $exe)) { throw "owa-probe.exe not found: $exe (run cargo build first)" }
$owaHost = 'mail.ngenergo.ru'
$user = Read-Host 'OWA login exactly as in the logon form (DOMAIN\user or user@domain)'
if ([string]::IsNullOrWhiteSpace($user)) { throw 'empty login' }
"AIOWA_HOST=$owaHost`r`nAIOWA_USER=$user" | Set-Content -Encoding UTF8 (Join-Path $here '.env.local')
& $exe --host $owaHost --user $user set-password
