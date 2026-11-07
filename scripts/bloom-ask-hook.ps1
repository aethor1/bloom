# Claude Code PreToolUse hook for AskUserQuestion: lets the Bloom notch show the
# question with its options and answer it with a click.
#
# Claude Code -> stdin JSON -> asks\<id>.json (Bloom shows it) -> Bloom writes
# asks\<id>.answer.json -> this script hands the answer back to Claude.
# If Bloom isn't running, the user picks "Answer in app", or nobody answers in
# time, the script exits silently and Claude Code shows its normal question UI.

$ErrorActionPreference = 'Stop'
$waitSeconds = 300
try {
    $payload = [Console]::In.ReadToEnd() | ConvertFrom-Json
    if ($payload.tool_name -ne 'AskUserQuestion') { exit 0 }
    if (-not (Get-Process bloom -ErrorAction SilentlyContinue)) { exit 0 }

    $dir = Join-Path $env:APPDATA 'com.sehaz.bloom\asks'
    New-Item -ItemType Directory -Force $dir | Out-Null
    $id = 'claude-' + [guid]::NewGuid().ToString('N').Substring(0, 12)
    $request = @{
        id        = $id
        agent     = 'claude'
        session   = $payload.session_id
        cwd       = $payload.cwd
        questions = $payload.tool_input.questions
    } | ConvertTo-Json -Depth 8 -Compress
    $reqPath = Join-Path $dir "$id.json"
    $ansPath = Join-Path $dir "$id.answer.json"
    [IO.File]::WriteAllText("$reqPath.tmp", $request, [Text.UTF8Encoding]::new($false))
    Move-Item -Force "$reqPath.tmp" $reqPath

    $deadline = (Get-Date).AddSeconds($waitSeconds)
    $answer = $null
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $ansPath) {
            $answer = Get-Content $ansPath -Raw -Encoding UTF8 | ConvertFrom-Json
            break
        }
        Start-Sleep -Milliseconds 200
    }
    Remove-Item -Force $reqPath, $ansPath -ErrorAction SilentlyContinue

    if ($null -eq $answer -or $null -eq $answer.answers) { exit 0 }  # fall back to the app's own UI

    $lines = foreach ($p in $answer.answers.PSObject.Properties) { "- `"$($p.Name)`" -> $($p.Value)" }
    $reason = "The user already answered this question from the Bloom notch on their desktop, so the question UI was skipped. Their answers:`n" +
        ($lines -join "`n") + "`nTreat these as the user's answers and continue; do not ask the same question again."
    @{
        hookSpecificOutput = @{
            hookEventName            = 'PreToolUse'
            permissionDecision       = 'deny'
            permissionDecisionReason = $reason
        }
    } | ConvertTo-Json -Depth 5 -Compress
    exit 0
} catch {
    if ($reqPath) { Remove-Item -Force $reqPath, $ansPath -ErrorAction SilentlyContinue }
    exit 0  # never block Claude because of the notch
}
