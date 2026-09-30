param(
    [string]$Source = 'C:\xstore',
    [string]$Destination = 'C:\xstore-sample',
    [int]$Seed = 20260929,
    [int]$AdditionalCount = 15
)

$ErrorActionPreference = 'Stop'
$sourceRoot = (Resolve-Path -LiteralPath $Source).Path.TrimEnd('\')
$corpusRoot = Join-Path $sourceRoot 'WORKSPACE_XSTORE.19.0.4'
if (-not (Test-Path -LiteralPath $corpusRoot -PathType Container)) {
    throw "Corpus directory not found: $corpusRoot"
}
if (Test-Path -LiteralPath $Destination) {
    throw "Destination already exists; refusing to overwrite: $Destination"
}
if ($AdditionalCount -lt 15) {
    throw 'AdditionalCount must be at least 15.'
}

$archives = @(Get-ChildItem -LiteralPath $corpusRoot -Recurse -File | Where-Object {
    $_.Extension -in '.zip', '.jar', '.war', '.aar'
})
$jar = $archives | Where-Object { $_.Name -eq 'installx-19.0.4.0.60.jar' } |
    Sort-Object Length -Descending | Select-Object -First 1
if (-not $jar) {
    throw 'installx-19.0.4.0.60.jar not found in the corpus.'
}
$bundle = Get-ChildItem -LiteralPath $sourceRoot -Recurse -File -Filter '*.appxbundle' -ErrorAction SilentlyContinue |
    Sort-Object Length -Descending | Select-Object -First 1
if (-not $bundle) {
    throw 'No .appxbundle found under the source root.'
}
$others = @($archives | Where-Object { $_.FullName -ne $jar.FullName } | Sort-Object FullName)
if ($others.Count -lt $AdditionalCount) {
    throw "Only $($others.Count) other archives found; need $AdditionalCount."
}
$selected = @($jar, $bundle) + @($others | Get-Random -Count $AdditionalCount -SetSeed $Seed)
New-Item -ItemType Directory -Path $Destination -ErrorAction Stop | Out-Null
$destinationRoot = (Resolve-Path -LiteralPath $Destination).Path
foreach ($archive in $selected) {
    $relativePath = $archive.FullName.Substring($sourceRoot.Length).TrimStart('\')
    $target = Join-Path $destinationRoot $relativePath
    $parent = Split-Path -Parent $target
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
    Copy-Item -LiteralPath $archive.FullName -Destination $target -ErrorAction Stop
}
$totalBytes = ($selected | Measure-Object -Property Length -Sum).Sum
Write-Host "Copied $($selected.Count) archives ($([math]::Round($totalBytes / 1MB, 2)) MiB) to $destinationRoot (seed $Seed)."
