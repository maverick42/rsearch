param(
    [string]$Ico = "crates/gui/assets/rsearch.ico",
    [string]$OutDir = "crates/gui/assets",
    [int[]]$Sizes = @(48, 128)
)
$b = [IO.File]::ReadAllBytes($Ico)
$count = [BitConverter]::ToUInt16($b, 4)
for ($i = 0; $i -lt $count; $i++) {
    $o = 6 + 16 * $i
    $w = $b[$o]; if ($w -eq 0) { $w = 256 }
    $h = $b[$o + 1]; if ($h -eq 0) { $h = 256 }
    $size = [BitConverter]::ToUInt32($b, $o + 8)
    $off = [BitConverter]::ToUInt32($b, $o + 12)
    if ($Sizes -contains $w -and $w -eq $h) {
        $name = Join-Path $OutDir ("rsearch-icon-{0}.png" -f $w)
        [IO.File]::WriteAllBytes($name, $b[$off..($off + $size - 1)])
        Write-Output "wrote $name ($size bytes)"
    }
}
