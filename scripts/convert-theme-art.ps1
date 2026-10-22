# Convert colour theme art in art/source/themes/<theme>/ into compact palette images for the app.
# Inputs per theme: states.png (3x2 grid), items.png (6x4 grid) and an optional seamless tile.png.
# Format (.nbp): "NBP1", u16 LE width, u16 LE height, u16 LE palette size, palette as RGBA, raw DEFLATE of 1-byte indices.
# Index 0 is always transparent. Each cell is cropped to its subject so every icon fills its slot evenly.
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$source = Join-Path $root 'art/source/themes'
$preview = Join-Path $root 'art/preview/themes'
$output = Join-Path $root 'crates/netburrow-app/assets/themes'
$references = if ($PSVersionTable.PSEdition -eq 'Core') { @('System.Drawing.Common', 'System.Drawing.Primitives') } else { @('System.Drawing') }
Add-Type -ReferencedAssemblies $references -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;
using System.IO;
using System.IO.Compression;
using System.Runtime.InteropServices;

public static class ThemeArt {
    static byte[] Bgra(Bitmap bitmap) {
        var data = bitmap.LockBits(new Rectangle(0, 0, bitmap.Width, bitmap.Height), ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
        var bytes = new byte[bitmap.Width * bitmap.Height * 4];
        for (int y = 0; y < bitmap.Height; y++) Marshal.Copy(data.Scan0 + y * data.Stride, bytes, y * bitmap.Width * 4, bitmap.Width * 4);
        bitmap.UnlockBits(data);
        return bytes;
    }

    static Bitmap FromBgra(byte[] bytes, int width, int height) {
        var bitmap = new Bitmap(width, height, PixelFormat.Format32bppArgb);
        var data = bitmap.LockBits(new Rectangle(0, 0, width, height), ImageLockMode.WriteOnly, PixelFormat.Format32bppArgb);
        for (int y = 0; y < height; y++) Marshal.Copy(bytes, y * width * 4, data.Scan0 + y * data.Stride, width * 4);
        bitmap.UnlockBits(data);
        return bitmap;
    }

    // Clears the flat sheet background reachable from the cell borders, so enclosed light colours survive.
    // Cleared pixels keep a dark ink colour, which keeps resampled edges from glowing on dark themes.
    static void KeyOut(byte[] px, int w, int h, int cols, int rows) {
        int bg = (8 * w + 8) * 4;
        int b0 = px[bg], g0 = px[bg + 1], r0 = px[bg + 2];
        var seen = new bool[w * h];
        var stack = new Stack<int>();
        Action<int, int> push = (x, y) => {
            int k = y * w + x;
            if (seen[k]) return;
            int o = k * 4, db = px[o] - b0, dg = px[o + 1] - g0, dr = px[o + 2] - r0;
            if (db * db + dg * dg + dr * dr > 34 * 34) return;
            seen[k] = true; stack.Push(k);
        };
        for (int i = 0; i <= cols; i++) { int x = Math.Min(w - 1, i * w / cols); for (int y = 0; y < h; y++) push(x, y); }
        for (int j = 0; j <= rows; j++) { int y = Math.Min(h - 1, j * h / rows); for (int x = 0; x < w; x++) push(x, y); }
        while (stack.Count > 0) {
            int k = stack.Pop(), x = k % w, y = k / w, o = k * 4;
            px[o] = 10; px[o + 1] = 14; px[o + 2] = 20; px[o + 3] = 0;
            if (x > 0) push(x - 1, y); if (x < w - 1) push(x + 1, y);
            if (y > 0) push(x, y - 1); if (y < h - 1) push(x, y + 1);
        }
    }

    // Subject bounds inside a cell, ignoring stray specks: a row or column needs a few opaque pixels.
    static Rectangle Bounds(byte[] px, int w, Rectangle cell) {
        int left = int.MaxValue, top = int.MaxValue, right = -1, bottom = -1;
        for (int y = cell.Top; y < cell.Bottom; y++) {
            int count = 0;
            for (int x = cell.Left; x < cell.Right; x++) if (px[(y * w + x) * 4 + 3] > 64) count++;
            if (count >= 3) { top = Math.Min(top, y); bottom = Math.Max(bottom, y); }
        }
        for (int x = cell.Left; x < cell.Right; x++) {
            int count = 0;
            for (int y = cell.Top; y < cell.Bottom; y++) if (px[(y * w + x) * 4 + 3] > 64) count++;
            if (count >= 3) { left = Math.Min(left, x); right = Math.Max(right, x); }
        }
        return right < 0 ? cell : Rectangle.FromLTRB(left, top, right + 1, bottom + 1);
    }

    static Graphics Quality(Bitmap target, bool pixel) {
        var g = Graphics.FromImage(target);
        g.InterpolationMode = pixel ? InterpolationMode.HighQualityBilinear : InterpolationMode.HighQualityBicubic;
        g.PixelOffsetMode = PixelOffsetMode.HighQuality;
        g.CompositingQuality = CompositingQuality.HighQuality;
        return g;
    }

    // Builds the atlas: each cell's subject is centred and fitted into a square slot of `cell` pixels.
    public static Bitmap Atlas(string path, int cols, int rows, int cell, bool pixel) {
        using (var original = new Bitmap(path)) {
            int w = original.Width, h = original.Height;
            var px = Bgra(original);
            KeyOut(px, w, h, cols, rows);
            var atlas = new Bitmap(cols * cell, rows * cell, PixelFormat.Format32bppArgb);
            using (var keyed = FromBgra(px, w, h))
            using (var g = Quality(atlas, pixel))
            using (var attributes = new ImageAttributes()) {
                attributes.SetWrapMode(WrapMode.Clamp);
                for (int r = 0; r < rows; r++)
                    for (int c = 0; c < cols; c++) {
                        var area = Rectangle.FromLTRB(c * w / cols, r * h / rows, (c + 1) * w / cols, (r + 1) * h / rows);
                        var subject = Bounds(px, w, area);
                        int side = (int)Math.Ceiling(Math.Max(subject.Width, subject.Height) * 1.04);
                        var square = new Rectangle(subject.Left + subject.Width / 2 - side / 2, subject.Top + subject.Height / 2 - side / 2, side, side);
                        square.Intersect(area);
                        float scale = (float)cell / side;
                        float dx = c * cell + (cell - square.Width * scale) / 2, dy = r * cell + (cell - square.Height * scale) / 2;
                        g.DrawImage(keyed, new Rectangle((int)Math.Round(dx), (int)Math.Round(dy), (int)Math.Round(square.Width * scale), (int)Math.Round(square.Height * scale)),
                            square.Left, square.Top, square.Width, square.Height, GraphicsUnit.Pixel, attributes);
                    }
            }
            if (pixel) {
                var bytes = Bgra(atlas);
                for (int i = 3; i < bytes.Length; i += 4) bytes[i] = bytes[i] < 128 ? (byte)0 : (byte)255;
                atlas.Dispose();
                return FromBgra(bytes, cols * cell, rows * cell);
            }
            return atlas;
        }
    }

    public static Bitmap Tile(string path, int size, bool pixel) {
        using (var original = new Bitmap(path)) {
            var tile = new Bitmap(size, size, PixelFormat.Format32bppArgb);
            using (var g = Quality(tile, pixel))
            using (var attributes = new ImageAttributes()) {
                attributes.SetWrapMode(WrapMode.Tile);
                g.DrawImage(original, new Rectangle(0, 0, size, size), 0, 0, original.Width, original.Height, GraphicsUnit.Pixel, attributes);
            }
            return tile;
        }
    }

    // Popularity quantizer on 5-bit colour and 3-bit alpha buckets; every pixel maps to its nearest entry.
    public static void Write(Bitmap image, string path, int maxColors) {
        int w = image.Width, h = image.Height;
        var px = Bgra(image);
        var buckets = new Dictionary<int, long[]>();
        Func<int, int> key = o => ((px[o + 2] >> 3) << 13) | ((px[o + 1] >> 3) << 8) | ((px[o] >> 3) << 3) | (px[o + 3] >> 5);
        for (int o = 0; o < px.Length; o += 4) {
            if (px[o + 3] < 16) continue;
            int k = key(o);
            long[] sum;
            if (!buckets.TryGetValue(k, out sum)) { sum = new long[5]; buckets[k] = sum; }
            sum[0]++; sum[1] += px[o + 2]; sum[2] += px[o + 1]; sum[3] += px[o]; sum[4] += px[o + 3];
        }
        var ordered = new List<long[]>(buckets.Values);
        ordered.Sort((a, b) => b[0].CompareTo(a[0]));
        int count = Math.Min(ordered.Count, maxColors - 1);
        var palette = new byte[(count + 1) * 4];
        for (int i = 0; i < count; i++) {
            var s = ordered[i];
            for (int c = 0; c < 4; c++) palette[(i + 1) * 4 + c] = (byte)(s[c + 1] / s[0]);
        }
        var nearest = new Dictionary<int, byte>();
        var indices = new byte[w * h];
        for (int o = 0, i = 0; o < px.Length; o += 4, i++) {
            if (px[o + 3] < 16) continue;
            int k = key(o);
            byte best;
            if (!nearest.TryGetValue(k, out best)) {
                best = 1;
                long bestDistance = long.MaxValue;
                for (int p = 1; p <= count; p++) {
                    long dr = palette[p * 4] - px[o + 2], dg = palette[p * 4 + 1] - px[o + 1], db = palette[p * 4 + 2] - px[o], da = palette[p * 4 + 3] - px[o + 3];
                    long distance = dr * dr * 3 + dg * dg * 4 + db * db * 2 + da * da * 3;
                    if (distance < bestDistance) { bestDistance = distance; best = (byte)p; }
                }
                nearest[k] = best;
            }
            indices[i] = best;
        }
        using (var file = File.Create(path)) {
            int entries = count + 1;
            var header = new byte[] { (byte)'N', (byte)'B', (byte)'P', (byte)'1', (byte)w, (byte)(w >> 8), (byte)h, (byte)(h >> 8), (byte)entries, (byte)(entries >> 8) };
            file.Write(header, 0, header.Length);
            file.Write(palette, 0, palette.Length);
            using (var deflate = new DeflateStream(file, CompressionLevel.Optimal)) deflate.Write(indices, 0, indices.Length);
        }
    }
}
'@

# Cell sizes are about twice the largest on-screen size; pixel art keeps its native grid and is scaled with nearest sampling.
$themes = @(
    @{ Name = 'paper';   Pixel = $false; State = 192; Item = 64; Tile = 256; Colors = 160; TileColors = 32 },
    @{ Name = 'pixel';   Pixel = $true;  State = 48;  Item = 24; Tile = 128; Colors = 64;  TileColors = 16 },
    @{ Name = 'cartoon'; Pixel = $false; State = 192; Item = 64; Tile = 0;   Colors = 160; TileColors = 0 }
)
New-Item -ItemType Directory -Force $preview | Out-Null
foreach ($theme in $themes) {
    $in = Join-Path $source $theme.Name
    $out = Join-Path $output $theme.Name
    New-Item -ItemType Directory -Force $out | Out-Null
    $jobs = @(
        @{ File = 'states'; Image = [ThemeArt]::Atlas((Join-Path $in 'states.png'), 3, 2, $theme.State, $theme.Pixel); Colors = $theme.Colors },
        @{ File = 'items'; Image = [ThemeArt]::Atlas((Join-Path $in 'items.png'), 6, 4, $theme.Item, $theme.Pixel); Colors = $theme.Colors }
    )
    if ($theme.Tile -gt 0) {
        $jobs += @{ File = 'tile'; Image = [ThemeArt]::Tile((Join-Path $in 'tile.png'), $theme.Tile, $theme.Pixel); Colors = $theme.TileColors }
    }
    foreach ($job in $jobs) {
        $target = Join-Path $out "$($job.File).nbp"
        [ThemeArt]::Write($job.Image, $target, $job.Colors)
        $job.Image.Save((Join-Path $preview "$($theme.Name)-$($job.File).png"), [System.Drawing.Imaging.ImageFormat]::Png)
        $job.Image.Dispose()
        '{0,-8} {1,-7} {2,7:N1} KB' -f $theme.Name, $job.File, ((Get-Item $target).Length / 1KB)
    }
}
