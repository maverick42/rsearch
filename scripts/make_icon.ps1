# Generates crates/gui/assets/app-icon.ico: a white magnifier on the UI
# accent-blue rounded square, at the usual icon sizes (16..256), packed
# as PNG entries (Vista+). Dev tool — re-run after tweaking the drawing.
param(
    [string]$Out = "crates\gui\assets\app-icon.ico",
    [string]$PngOut = "crates\gui\ui\icons\app-icon.png"
)

Add-Type -AssemblyName System.Drawing

$sizes = 16, 24, 32, 48, 64, 128, 256
$images = foreach ($n in $sizes) {
    $bmp = New-Object System.Drawing.Bitmap $n, $n
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.Clear([System.Drawing.Color]::Transparent)

    # Rounded-square background in the app accent color (#3B82F6).
    $m = $n * 0.05
    $d = $n - 2 * $m
    $r = $n * 0.22
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc($m, $m, 2 * $r, 2 * $r, 180, 90)
    $path.AddArc($m + $d - 2 * $r, $m, 2 * $r, 2 * $r, 270, 90)
    $path.AddArc($m + $d - 2 * $r, $m + $d - 2 * $r, 2 * $r, 2 * $r, 0, 90)
    $path.AddArc($m, $m + $d - 2 * $r, 2 * $r, 2 * $r, 90, 90)
    $path.CloseFigure()
    $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(59, 130, 246))
    $g.FillPath($brush, $path)

    # Magnifier: circle + handle pointing down-right.
    $penWidth = [Math]::Max(1.5, $n * 0.085)
    $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::White), $penWidth
    $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
    $cx = $n * 0.44
    $cy = $n * 0.42
    $cr = $n * 0.20
    $g.DrawEllipse($pen, $cx - $cr, $cy - $cr, 2 * $cr, 2 * $cr)
    $g.DrawLine($pen,
        $cx + $cr * 0.72, $cy + $cr * 0.72,
        $cx + $cr * 1.85, $cy + $cr * 1.85)

    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    , $ms.ToArray()
    $g.Dispose()
    $bmp.Dispose()
}

# ICO container: ICONDIR + one ICONDIRENTRY per image, then the PNG data.
$dir = Split-Path $Out -Parent
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
$fs = [System.IO.File]::Create($Out)
$bw = New-Object System.IO.BinaryWriter $fs
$bw.Write([uint16]0)
$bw.Write([uint16]1)
$bw.Write([uint16]$images.Count)
$offset = 6 + 16 * $images.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $n = $sizes[$i]
    $bw.Write([byte]($(if ($n -ge 256) { 0 } else { $n })))
    $bw.Write([byte]($(if ($n -ge 256) { 0 } else { $n })))
    $bw.Write([byte]0)
    $bw.Write([byte]0)
    $bw.Write([uint16]1)
    $bw.Write([uint16]32)
    $bw.Write([uint32]$images[$i].Length)
    $bw.Write([uint32]$offset)
    $offset += $images[$i].Length
}
foreach ($img in $images) { $bw.Write($img) }
$bw.Close()

# A PNG sibling for the Slint `icon` property (@image-url wants an
# image file, not the .ico container).
$pngDir = Split-Path $PngOut -Parent
if ($pngDir -and -not (Test-Path $pngDir)) { New-Item -ItemType Directory -Path $pngDir | Out-Null }
[IO.File]::WriteAllBytes($PngOut, $images[$images.Count - 1])
Write-Output "wrote $Out"
Write-Output "wrote $PngOut"
