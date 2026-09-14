# Regenerate the application icon without additional dependencies.
Add-Type -AssemblyName System.Drawing
$assetRoot = Join-Path $PSScriptRoot '../crates/netburrow-app/assets'
$images = @()
foreach ($size in @(16, 20, 24, 32, 48, 64, 128, 256)) {
    $bitmap = [Drawing.Bitmap]::new($size, $size)
    $g = [Drawing.Graphics]::FromImage($bitmap)
    $g.SmoothingMode = [Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.ScaleTransform($size / 48.0, $size / 48.0)
    $mint = [Drawing.SolidBrush]::new([Drawing.Color]::FromArgb(114, 215, 190))
    $dark = [Drawing.SolidBrush]::new([Drawing.Color]::FromArgb(20, 26, 35))
    $path = [Drawing.Drawing2D.GraphicsPath]::new()
    $path.AddArc(0, 0, 24, 24, 180, 90)
    $path.AddArc(24, 0, 24, 24, 270, 90)
    $path.AddArc(24, 24, 24, 24, 0, 90)
    $path.AddArc(0, 24, 24, 24, 90, 90)
    $path.CloseFigure()
    $g.FillPath($mint, $path)
    $pen = [Drawing.Pen]::new($dark, 5)
    $g.DrawLine($pen, 14, 33, 14, 23)
    $g.DrawArc($pen, 14, 13, 20, 20, 180, 180)
    $g.DrawLine($pen, 34, 23, 34, 33)
    $g.FillEllipse($dark, 10, 30, 8, 8)
    $g.FillEllipse($dark, 30, 30, 8, 8)
    $stream = [IO.MemoryStream]::new()
    $bitmap.Save($stream, [Drawing.Imaging.ImageFormat]::Png)
    $images += ,@($size, $stream.ToArray())
    if ($size -eq 256) { [IO.File]::WriteAllBytes((Join-Path $assetRoot 'netburrow.png'), $stream.ToArray()) }
    if ($size -eq 64) {
        $rgba = [Collections.Generic.List[byte]]::new()
        for ($y = 0; $y -lt 64; $y++) {
            for ($x = 0; $x -lt 64; $x++) {
                $c = $bitmap.GetPixel($x, $y)
                $rgba.AddRange([byte[]]@($c.R, $c.G, $c.B, $c.A))
            }
        }
        [IO.File]::WriteAllBytes((Join-Path $assetRoot 'netburrow.rgba'), $rgba.ToArray())
    }
    $stream.Dispose(); $pen.Dispose(); $path.Dispose(); $mint.Dispose(); $dark.Dispose(); $g.Dispose(); $bitmap.Dispose()
}
$file = [IO.File]::Create((Join-Path $assetRoot 'netburrow.ico'))
$writer = [IO.BinaryWriter]::new($file)
$writer.Write([uint16]0); $writer.Write([uint16]1); $writer.Write([uint16]$images.Count)
$offset = 6 + 16 * $images.Count
foreach ($entry in $images) {
    $dimension = $entry[0] % 256
    $writer.Write([byte]$dimension); $writer.Write([byte]$dimension)
    $writer.Write([uint16]0); $writer.Write([uint16]1); $writer.Write([uint16]32)
    $writer.Write([uint32]$entry[1].Length); $writer.Write([uint32]$offset)
    $offset += $entry[1].Length
}
foreach ($entry in $images) { $writer.Write([byte[]]$entry[1]) }
$writer.Dispose()
