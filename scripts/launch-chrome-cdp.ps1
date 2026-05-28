# Launch Chrome with the DevTools debugging port open so JARVIS can
# attach to your real interactive session (background tabs included).
#
# Usage:
#   pwsh -NoProfile -ExecutionPolicy Bypass -File launch-chrome-cdp.ps1
#   pwsh -File launch-chrome-cdp.ps1 -Profile "Default"
#   pwsh -File launch-chrome-cdp.ps1 -Profile "Profile 3" -Port 9222
#
# Close ALL Chrome windows first — Chrome only honors the debug-port flag
# at process start, and only one Chrome process per user-data-dir can run
# at a time. After you close everything and run this script, Chrome
# reopens with debug enabled; from then on JARVIS's `playwright_cdp` MCP
# can drive your real tabs.
#
# Security: the debug port is bound to localhost only by default, so
# nothing on the network can reach it. But ANY process on this machine
# can drive Chrome through it. Don't leave it on for sessions where
# you're logged into things you don't want Iron Clad to see.

param(
    [string]$Profile = "Default",
    [int]$Port = 9222
)

$chrome = "$env:ProgramFiles\Google\Chrome\Application\chrome.exe"
if (-not (Test-Path $chrome)) {
    $chrome = "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe"
}
if (-not (Test-Path $chrome)) {
    Write-Host "Chrome not found in Program Files. Edit this script with your chrome.exe path." -ForegroundColor Red
    exit 1
}

$userData = "$env:LOCALAPPDATA\Google\Chrome\User Data"

# Refuse to launch if Chrome is already running against the same data dir;
# it would silently ignore --remote-debugging-port. Tell McKale instead.
$existing = Get-Process chrome -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "Chrome is already running ($($existing.Count) processes)." -ForegroundColor Yellow
    Write-Host "Close ALL Chrome windows first, then re-run this script. The debug" -ForegroundColor Yellow
    Write-Host "port only takes effect at process start." -ForegroundColor Yellow
    Write-Host ""
    Write-Host "Force-kill all Chrome? (y/n): " -NoNewline -ForegroundColor Yellow
    $resp = Read-Host
    if ($resp -ne "y") { exit 1 }
    Stop-Process -Name chrome -Force
    Start-Sleep -Seconds 2
}

$args = @(
    "--remote-debugging-port=$Port",
    "--remote-allow-origins=*",
    "--user-data-dir=$userData",
    "--profile-directory=$Profile"
)
Write-Host "Launching Chrome with debug port $Port on profile '$Profile'..." -ForegroundColor Green
Start-Process -FilePath $chrome -ArgumentList $args

Start-Sleep -Seconds 2
Write-Host ""
Write-Host "Chrome should now be reachable at http://localhost:$Port" -ForegroundColor Green
Write-Host "Next time you 'cargo run-jarvis', the playwright_cdp MCP will auto-attach" -ForegroundColor Green
Write-Host "and JARVIS will see all your interactive tabs." -ForegroundColor Green
