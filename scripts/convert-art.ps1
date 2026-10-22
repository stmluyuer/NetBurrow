# Convert ink line art in art/source/ into tintable alpha masks for the app, without extra dependencies.
# Mask format (.mask): "NBM1", u16 LE width, u16 LE height, raw DEFLATE of width*height alpha bytes.
# The app uploads it as white+alpha and tints it at draw time, so one asset serves every state color.
# -AppIcon also rewrites netburrow.ico/.png/.rgba from icon-glyph.png (<= 48 px) and logo.png (larger sizes).
param([switch]$AppIcon)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$source = Join-Path $root 'art/source'
$preview = Join-Path $root 'art/preview'
$appAssets = Join-Path $root 'crates/netburrow-app/assets'
$maskDir = Join-Path $appAssets 'art'
$references = if ($PSVersionTable.PSEdition -eq 'Core') { @('System.Drawing.Common', 'System.Drawing.Primitives') } else { @('System.Drawing') }
Add-Type -ReferencedAssemblies $references -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;
using System.IO;
using System.IO.Compression;
using System.Runtime.InteropServices;

public static class NetBurrowArt {
    // Paper at or above this luminance becomes transparent; ink at or below it becomes opaque.
    const double Paper = 232, Ink = 48;

    static byte[] Argb(Bitmap bitmap) {
        var rect = new Rectangle(0, 0, bitmap.Width, bitmap.Height);
        var data = bitmap.LockBits(rect, ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
        var bytes = new byte[data.Stride * data.Height];
        Marshal.Copy(data.Scan0, bytes, 0, bytes.Length);
        bitmap.UnlockBits(data);
        return bytes;
    }

    static Bitmap FromAlpha(byte[] alpha, int width, int height, Color ink) {
        var bitmap = new Bitmap(width, height, PixelFormat.Format32bppArgb);
        var data = bitmap.LockBits(new Rectangle(0, 0, width, height), ImageLockMode.WriteOnly, PixelFormat.Format32bppArgb);
        var bytes = new byte[data.Stride * height];
        for (int y = 0; y < height; y++)
            for (int x = 0; x < width; x++) {
                int o = y * data.Stride + x * 4;
                bytes[o] = ink.B; bytes[o + 1] = ink.G; bytes[o + 2] = ink.R; bytes[o + 3] = alpha[y * width + x];
            }
        Marshal.Copy(bytes, 0, data.Scan0, bytes.Length);
        bitmap.UnlockBits(data);
        return bitmap;
    }

    // Returns ink coverage cropped to its content, fitted inside maxWidth x maxHeight.
    public static byte[] Mask(string path, int maxWidth, int maxHeight, out int width, out int height) {
        using (var original = new Bitmap(path)) {
            int w = original.Width, h = original.Height;
            var argb = Argb(original);
            var coverage = new byte[w * h];
            int left = w, top = h, right = -1, bottom = -1;
            for (int y = 0; y < h; y++)
                for (int x = 0; x < w; x++) {
                    int o = (y * w + x) * 4;
                    double luminance = 0.299 * argb[o + 2] + 0.587 * argb[o + 1] + 0.114 * argb[o];
                    double value = (Paper - luminance) / (Paper - Ink);
                    value = Math.Max(0, Math.Min(1, value)) * argb[o + 3] / 255.0;
                    byte a = (byte)Math.Round(value * 255);
                    coverage[y * w + x] = a;
                    if (a > 24) { left = Math.Min(left, x); right = Math.Max(right, x); top = Math.Min(top, y); bottom = Math.Max(bottom, y); }
                }
            if (right < 0) throw new InvalidDataException("No ink found in " + path);
            int pad = Math.Max(right - left, bottom - top) / 40;
            left = Math.Max(0, left - pad); top = Math.Max(0, top - pad);
            right = Math.Min(w - 1, right + pad); bottom = Math.Min(h - 1, bottom + pad);
            int cw = right - left + 1, ch = bottom - top + 1;
            var cropped = new byte[cw * ch];
            for (int y = 0; y < ch; y++) Buffer.BlockCopy(coverage, (y + top) * w + left, cropped, y * cw, cw);
            double scale = Math.Min(1.0, Math.Min((double)maxWidth / cw, (double)maxHeight / ch));
            width = Math.Max(1, (int)Math.Round(cw * scale));
            height = Math.Max(1, (int)Math.Round(ch * scale));
            using (var full = FromAlpha(cropped, cw, ch, Color.Black))
            using (var small = Resize(full, width, height)) {
                var resized = Argb(small);
                var alpha = new byte[width * height];
                for (int i = 0; i < alpha.Length; i++) alpha[i] = resized[i * 4 + 3];
                return alpha;
            }
        }
    }

    static Bitmap Resize(Bitmap image, int width, int height) {
        var result = new Bitmap(width, height, PixelFormat.Format32bppArgb);
        using (var g = Graphics.FromImage(result)) {
            g.InterpolationMode = InterpolationMode.HighQualityBicubic;
            g.PixelOffsetMode = PixelOffsetMode.HighQuality;
            g.CompositingQuality = CompositingQuality.HighQuality;
            using (var wrap = new ImageAttributes()) {
                wrap.SetWrapMode(WrapMode.TileFlipXY);
                g.DrawImage(image, new Rectangle(0, 0, width, height), 0, 0, image.Width, image.Height, GraphicsUnit.Pixel, wrap);
            }
        }
        return result;
    }

    public static void WriteMask(string path, byte[] alpha, int width, int height) {
        using (var file = File.Create(path)) {
            var header = new byte[] { (byte)'N', (byte)'B', (byte)'M', (byte)'1', (byte)width, (byte)(width >> 8), (byte)height, (byte)(height >> 8) };
            file.Write(header, 0, header.Length);
            using (var deflate = new DeflateStream(file, CompressionLevel.Optimal)) deflate.Write(alpha, 0, alpha.Length);
        }
    }

    // Draws the mask in the app's ink colour on its background, at the size it will be shown.
    public static Bitmap Tinted(byte[] alpha, int width, int height, Color ink, int boxWidth, int boxHeight, Color background) {
        double scale = Math.Min((double)boxWidth / width, (double)boxHeight / height);
        int w = (int)Math.Round(width * scale), h = (int)Math.Round(height * scale);
        var result = new Bitmap(boxWidth, boxHeight, PixelFormat.Format32bppArgb);
        using (var g = Graphics.FromImage(result))
        using (var tinted = FromAlpha(alpha, width, height, ink))
        using (var small = Resize(tinted, w, h)) {
            g.Clear(background);
            g.DrawImage(small, (boxWidth - w) / 2, (boxHeight - h) / 2, w, h);
        }
        return result;
    }

    // App icon: ink on a rounded warm square, matching the in-app palette.
    public static Bitmap Icon(byte[] alpha, int width, int height, int size, Color ink, Color fill) {
        var result = new Bitmap(size, size, PixelFormat.Format32bppArgb);
        using (var g = Graphics.FromImage(result))
        using (var path = new GraphicsPath())
        using (var brush = new SolidBrush(fill)) {
            g.SmoothingMode = SmoothingMode.AntiAlias;
            float r = size * 0.22f, s = size - 1;
            path.AddArc(0, 0, r * 2, r * 2, 180, 90);
            path.AddArc(s - r * 2, 0, r * 2, r * 2, 270, 90);
            path.AddArc(s - r * 2, s - r * 2, r * 2, r * 2, 0, 90);
            path.AddArc(0, s - r * 2, r * 2, r * 2, 90, 90);
            path.CloseFigure();
            g.FillPath(brush, path);
            int box = (int)Math.Round(size * 0.78);
            using (var art = Tinted(alpha, width, height, ink, box, box, Color.Transparent))
                g.DrawImage(art, (size - box) / 2, (size - box) / 2, box, box);
        }
        return result;
    }
}
'@

$ink = [Drawing.Color]::FromArgb(52, 49, 45)        # TEXT
$paper = [Drawing.Color]::FromArgb(246, 243, 237)   # BACKGROUND
$card = [Drawing.Color]::FromArgb(251, 249, 245)    # raised card surface
# Mask box (px, ~3x the logical display size) and the logical size used for the review sheet.
$assets = [ordered]@{
    'logo'             = @{ Max = 192, 192; Show = 56, 56 }
    'state-idle'       = @{ Max = 288, 288; Show = 96, 96 }
    'state-connecting' = @{ Max = 288, 288; Show = 96, 96 }
    'state-waiting'    = @{ Max = 288, 288; Show = 96, 96 }
    'state-connected'  = @{ Max = 288, 288; Show = 96, 96 }
    'state-error'      = @{ Max = 288, 288; Show = 96, 96 }
    'empty-members'    = @{ Max = 288, 216; Show = 112, 84 }
    'empty-check'      = @{ Max = 288, 216; Show = 112, 84 }
}

New-Item -ItemType Directory -Force $maskDir, $preview | Out-Null
$sheetItems = @()
foreach ($name in $assets.Keys) {
    $file = Join-Path $source "$name.png"
    if (-not (Test-Path $file)) { Write-Warning "Missing $file"; continue }
    $spec = $assets[$name]
    $w = 0; $h = 0
    $alpha = [NetBurrowArt]::Mask($file, $spec.Max[0], $spec.Max[1], [ref]$w, [ref]$h)
    [NetBurrowArt]::WriteMask((Join-Path $maskDir "$name.mask"), $alpha, $w, $h)
    $full = [NetBurrowArt]::Tinted($alpha, $w, $h, $ink, $w, $h, $card)
    $full.Save((Join-Path $preview "$name.png"), [Drawing.Imaging.ImageFormat]::Png); $full.Dispose()
    $sheetItems += , @($name, $alpha, $w, $h, $spec.Show)
    '{0,-18} {1,4}x{2,-4} {3,7:N0} bytes' -f $name, $w, $h, (Get-Item (Join-Path $maskDir "$name.mask")).Length
}

# Review sheet: every asset at 1x and 1.5x logical size, plus the icon at its real pixel sizes.
$glyphFile = Join-Path $source 'icon-glyph.png'
$logoFile = Join-Path $source 'logo.png'
$icons = @()
if ((Test-Path $glyphFile) -and (Test-Path $logoFile)) {
    $gw = 0; $gh = 0; $lw = 0; $lh = 0
    $glyph = [NetBurrowArt]::Mask($glyphFile, 256, 256, [ref]$gw, [ref]$gh)
    $logo = [NetBurrowArt]::Mask($logoFile, 256, 256, [ref]$lw, [ref]$lh)
    foreach ($size in @(16, 20, 24, 32, 48, 64, 128, 256)) {
        $icon = if ($size -le 48) { [NetBurrowArt]::Icon($glyph, $gw, $gh, $size, $ink, $paper) } else { [NetBurrowArt]::Icon($logo, $lw, $lh, $size, $ink, $paper) }
        $icons += , @($size, $icon)
    }
}
$sheet = [Drawing.Bitmap]::new(1460, 560)
$g = [Drawing.Graphics]::FromImage($sheet)
$g.Clear($paper)
foreach ($scale in @(1.0, 1.5)) {
    $x = 16; $y = if ($scale -eq 1.0) { 16 } else { 150 }
    foreach ($item in $sheetItems) {
        $bw = [int]($item[4][0] * $scale); $bh = [int]($item[4][1] * $scale)
        $tile = [NetBurrowArt]::Tinted($item[1], $item[2], $item[3], $ink, $bw, $bh, $card)
        $g.DrawImage($tile, $x, $y, $bw, $bh); $tile.Dispose()
        $x += $bw + 14
    }
}
$g.FillRectangle([Drawing.SolidBrush]::new([Drawing.Color]::FromArgb(32, 32, 32)), 0, 316, 1460, 244)  # taskbar-like strip
$x = 16
foreach ($entry in $icons) {
    $g.DrawImage($entry[1], $x, 330, $entry[0], $entry[0])
    $x += $entry[0] + 14
}
$g.Dispose()
$sheet.Save((Join-Path $preview '_sheet.png'), [Drawing.Imaging.ImageFormat]::Png)
$sheet.Dispose()
"Review sheet: $(Join-Path $preview '_sheet.png')"

if ($AppIcon) {
    if ($icons.Count -eq 0) { throw 'icon-glyph.png and logo.png are required for -AppIcon' }
    $images = @()
    foreach ($entry in $icons) {
        $stream = [IO.MemoryStream]::new()
        $entry[1].Save($stream, [Drawing.Imaging.ImageFormat]::Png)
        $images += , @($entry[0], $stream.ToArray())
        if ($entry[0] -eq 256) { [IO.File]::WriteAllBytes((Join-Path $appAssets 'netburrow.png'), $stream.ToArray()) }
        $stream.Dispose()
    }
    # The window icon is shown at title-bar size, so it uses the bold glyph even though it is 64 px.
    $window = [NetBurrowArt]::Icon($glyph, $gw, $gh, 64, $ink, $paper)
    $rgba = [Collections.Generic.List[byte]]::new()
    for ($py = 0; $py -lt 64; $py++) {
        for ($px = 0; $px -lt 64; $px++) {
            $c = $window.GetPixel($px, $py)
            $rgba.AddRange([byte[]]@($c.R, $c.G, $c.B, $c.A))
        }
    }
    [IO.File]::WriteAllBytes((Join-Path $appAssets 'netburrow.rgba'), $rgba.ToArray())
    $window.Dispose()
    $file = [IO.File]::Create((Join-Path $appAssets 'netburrow.ico'))
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
    'App icon updated: netburrow.ico, netburrow.png, netburrow.rgba'
}
foreach ($entry in $icons) { $entry[1].Dispose() }
