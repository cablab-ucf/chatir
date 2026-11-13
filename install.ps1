# Self-updating chatir: irm $R/install.ps1 | iex
# -Server: an always-on storage node instead (a task, at login).
# Fetched by PowerShell, it is not marked as from the web: no SmartScreen.
# With minisign here, each file is checked against chatir's key as updates
# are: signed as itself, of this version. Without it nothing stock checks
# Ed25519 (.NET Framework has none), and a checker fetched here would be
# vouched for by GitHub's TLS alone: so unchecked is asked for, not silent.
param([switch]$Server)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$R = 'https://github.com/cablab-ucf/chatir/releases'
$KEY = 'RWQW5LH1agYG+mSdtDVXPOZJl9ANp4NS/IQ9lTqASnxSmJ4N33gv7Msy'
# The latest release, its files fetched by its tag: one release, whole.
$latest = Invoke-RestMethod "$R/latest/download/latest.json"
$v = $latest.version
if ($v -notmatch '^[0-9.]+$') { throw "chatir: no version: $v" }
$R = "$R/download/v$v"
$minisign = Get-Command minisign -ErrorAction SilentlyContinue
if (-not $minisign -and $env:CHATIR_UNCHECKED -ne '1') {
  Write-Warning ("No minisign here to check chatir's signature: only " +
    "GitHub's TLS would vouch for it. Install minisign (winget install " +
    "jedisct1.minisign) and run this again, or answer y " +
    "(CHATIR_UNCHECKED=1 says so ahead).")
  $a = ''
  try { $a = Read-Host 'Install unchecked? [y/N]' } catch {}
  if ($a -ne 'y') { throw 'chatir: not installed' }
}
function Get($url, $to) {
  Invoke-WebRequest $url -OutFile $to
  if (-not $minisign) { return }
  Invoke-WebRequest "$url.sig" -OutFile "$to.sig"
  $sig = [Convert]::FromBase64String([IO.File]::ReadAllText("$to.sig"))
  [IO.File]::WriteAllBytes("$to.minisig", $sig)
  $said = & $minisign -Vm $to -x "$to.minisig" -P $KEY
  Remove-Item "$to.sig", "$to.minisig"
  $as = "Trusted comment: *`tfile:$(Split-Path $url -Leaf)`tversion:$v"
  if ($LASTEXITCODE -or -not ($said -like $as)) {
    Remove-Item $to
    throw "${url}: not signed by chatir as v$v"
  }
}

if ($Server) {
  $dir = "$env:LOCALAPPDATA\chatir-server"
  New-Item -ItemType Directory -Force $dir | Out-Null
  Get "$R/chatir-x86_64-pc-windows-msvc.exe" "$dir\chatir.exe"
  & "$dir\chatir.exe" --install
  exit $LASTEXITCODE
}

# The installer, for you alone: no administrator, no questions.
$url = $latest.platforms."windows-x86_64-nsis".url
$setup = "$env:TEMP\chatir-setup.exe"
Get $url $setup
Start-Process $setup -ArgumentList '/S' -Wait
Remove-Item $setup
$exe = Get-ChildItem "$env:LOCALAPPDATA\chatir" -Filter chatir*.exe -Recurse |
  Where-Object Name -NotMatch 'uninstall' | Select-Object -First 1
if ($exe) { Start-Process $exe.FullName }
Write-Host 'chatir is in your Start menu, and updates itself.'
