<#
.SYNOPSIS
  Detect upstream PowerToys changes to the FancyZones code that fz-maximize mirrors.

.DESCRIPTION
  Compares the git blob SHA of every file listed in upstream/fancyzones-baseline.json
  against a PowerToys ref (default: main). For each changed file it prints which part of
  fz-maximize mirrors it and the upstream commits that touched it since the baseline.

  Exit codes: 0 = nothing changed, 1 = changes to review, 2 = error (network, rate limit).

  Uses GITHUB_TOKEN / GH_TOKEN, or `gh auth token`, when available (60 unauthenticated
  requests per hour are otherwise enough for one run).

.PARAMETER Ref
  PowerToys branch, tag or commit to compare against, e.g. main or v0.103.0.

.PARAMETER Update
  Re-pin the baseline to -Ref (run after reviewing the changes and updating the port).
#>
param(
    [string]$Ref = "main",
    [switch]$Update,
    [string]$Baseline = (Join-Path $PSScriptRoot "..\upstream\fancyzones-baseline.json")
)

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$base = Get-Content -Raw -Path $Baseline | ConvertFrom-Json
$repo = $base.repo
$headers = @{ "User-Agent" = "fz-maximize-upstream-check"; "Accept" = "application/vnd.github+json" }
$token = $env:GITHUB_TOKEN
if (-not $token) { $token = $env:GH_TOKEN }
if (-not $token -and (Get-Command gh -ErrorAction SilentlyContinue)) {
    $token = (& gh auth token 2>$null)
}
if ($token) { $headers["Authorization"] = "Bearer $($token.Trim())" }

function Invoke-GitHub([string]$path) {
    Invoke-RestMethod -Headers $headers -Uri "https://api.github.com/repos/$repo/$path"
}

try {
    $target = Invoke-GitHub "commits/$([uri]::EscapeDataString($Ref))"
} catch {
    Write-Host "error: cannot resolve '$Ref' in $repo : $($_.Exception.Message)" -ForegroundColor Red
    exit 2
}
$sha = $target.sha
Write-Host "Baseline : $repo@$($base.commit.Substring(0,10)) ($($base.commit_date), PowerToys $($base.powertoys_version))"
Write-Host "Comparing: $Ref = $($sha.Substring(0,10)) ($($target.commit.committer.date))"
Write-Host ""

$changed = @()
$errors = 0
foreach ($f in $base.files) {
    try {
        $now = (Invoke-GitHub "contents/$($f.path)?ref=$sha").sha
    } catch {
        $status = $_.Exception.Response.StatusCode.value__
        if ($status -eq 404) {
            Write-Host ("  MISSING   {0}  (moved or deleted upstream; review {1})" -f $f.path, $f.mirrors) -ForegroundColor Red
            $changed += [pscustomobject]@{ file = $f; blob = $null }
        } else {
            Write-Host ("  ERROR     {0}: {1}" -f $f.path, $_.Exception.Message) -ForegroundColor Red
            $errors++
        }
        continue
    }
    if ($now -eq $f.blob) {
        Write-Host ("  unchanged {0}" -f $f.path) -ForegroundColor DarkGray
        continue
    }
    $color = if ($f.impact -eq "high") { "Red" } elseif ($f.impact -eq "medium") { "Yellow" } else { "Cyan" }
    Write-Host ("  CHANGED   {0}  [{1} impact]" -f $f.path, $f.impact) -ForegroundColor $color
    Write-Host ("            what : {0}" -f $f.what)
    Write-Host ("            port : {0}" -f $f.mirrors)
    try {
        $since = [uri]::EscapeDataString($base.commit_date)
        $commits = Invoke-GitHub "commits?path=$([uri]::EscapeDataString($f.path))&sha=$sha&since=$since&per_page=10"
        foreach ($c in $commits) {
            if ($c.sha -eq $base.commit) { continue }
            $msg = ($c.commit.message -split "`n")[0]
            Write-Host ("            {0} {1}  {2}" -f $c.sha.Substring(0,10), $c.commit.committer.date.ToString().Substring(0,10), $msg)
        }
    } catch { }
    Write-Host ("            diff : https://github.com/$repo/compare/{0}...{1}" -f $base.commit, $sha)
    $changed += [pscustomobject]@{ file = $f; blob = $now }
}

Write-Host ""
if ($errors -gt 0) {
    Write-Host "$errors file(s) could not be checked (rate limit? set GITHUB_TOKEN)." -ForegroundColor Red
    exit 2
}

if ($Update) {
    foreach ($c in $changed) { if ($c.blob) { $c.file.blob = $c.blob } }
    $base.commit = $sha
    $base.commit_date = $target.commit.committer.date.ToString("yyyy-MM-ddTHH:mm:ssZ")
    $json = $base | ConvertTo-Json -Depth 5
    [IO.File]::WriteAllText((Resolve-Path $Baseline), $json + "`n")
    Write-Host "Baseline re-pinned to $Ref ($($sha.Substring(0,10)))." -ForegroundColor Green
    exit 0
}

if ($changed.Count -eq 0) {
    Write-Host "No upstream changes to mirrored FancyZones code since the baseline." -ForegroundColor Green
    exit 0
}
Write-Host "$($changed.Count) mirrored file(s) changed upstream. Review the port, then run 'task upstream:accept'." -ForegroundColor Yellow
exit 1
