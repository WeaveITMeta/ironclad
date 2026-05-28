# Seed IronClaw workspace with identity files from the Olson Life System
# Usage: pwsh scripts/seed_workspace.ps1

$seedDir = "$PSScriptRoot\..\workspace_seed"
$connStr = "-U postgres -h 127.0.0.1 -p 5432 -d ironclad --no-psqlrc --pset=pager=off"

$files = @(
    "USER.md",
    "SOUL.md",
    "IDENTITY.md",
    "AGENTS.md",
    "HEARTBEAT.md",
    "MEMORY.md",
    "README.md"
)

foreach ($file in $files) {
    $filePath = Join-Path $seedDir $file
    if (-not (Test-Path $filePath)) {
        Write-Host "SKIP: $file not found at $filePath" -ForegroundColor Yellow
        continue
    }

    $content = (Get-Content $filePath -Raw) -replace "'", "''"
    $sql = @"
INSERT INTO memory_documents (id, user_id, path, content, created_at, updated_at)
VALUES (gen_random_uuid(), 'default', '$file', '$content', NOW(), NOW())
ON CONFLICT (user_id, path) WHERE agent_id IS NULL
DO UPDATE SET content = EXCLUDED.content, updated_at = NOW();
"@

    $result = $sql | psql -U postgres -h 127.0.0.1 -p 5432 -d ironclad --no-psqlrc --pset=pager=off -f - 2>&1
    if ($LASTEXITCODE -eq 0) {
        Write-Host "OK: Seeded $file" -ForegroundColor Green
    } else {
        Write-Host "FAIL: $file — $result" -ForegroundColor Red
    }
}

Write-Host "`nDone. Verifying..." -ForegroundColor Cyan
psql -U postgres -h 127.0.0.1 -p 5432 -d ironclad --no-psqlrc --pset=pager=off -t -A -c "SELECT path, length(content) as chars FROM memory_documents WHERE user_id='default' ORDER BY path;"
