$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$assets = Join-Path (Split-Path -Parent $PSScriptRoot) 'assets'
$bitmap = [Drawing.Bitmap]::new((Join-Path $assets 'icon.png'))
try {
    $images = @(
        foreach ($size in @(16, 20, 22, 24, 32, 40, 48, 64, 128, 256)) {
            $scaled = [Drawing.Bitmap]::new($size, $size)
            $draw = [Drawing.Graphics]::FromImage($scaled)
            $stream = [IO.MemoryStream]::new()
            try {
                $draw.InterpolationMode = [Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
                $draw.PixelOffsetMode = [Drawing.Drawing2D.PixelOffsetMode]::HighQuality
                $draw.DrawImage($bitmap, 0, 0, $size, $size)
                $scaled.Save($stream, [Drawing.Imaging.ImageFormat]::Png)
                [pscustomobject]@{ Size = $size; Bytes = $stream.ToArray() }
            }
            finally {
                $stream.Dispose()
                $draw.Dispose()
                $scaled.Dispose()
            }
        }
    )

    $file = [IO.File]::Create((Join-Path $assets 'icon.ico'))
    $writer = [IO.BinaryWriter]::new($file)
    try {
        $writer.Write([uint16]0)
        $writer.Write([uint16]1)
        $writer.Write([uint16]$images.Count)
        $offset = 6 + 16 * $images.Count
        foreach ($image in $images) {
            $dimension = if ($image.Size -eq 256) { 0 } else { $image.Size }
            $writer.Write([byte]$dimension)
            $writer.Write([byte]$dimension)
            $writer.Write([uint16]0)
            $writer.Write([uint16]1)
            $writer.Write([uint16]32)
            $writer.Write([uint32]$image.Bytes.Length)
            $writer.Write([uint32]$offset)
            $offset += $image.Bytes.Length
        }
        foreach ($image in $images) { $writer.Write([byte[]]$image.Bytes) }
    }
    finally { $writer.Dispose() }
}
finally { $bitmap.Dispose() }

Write-Output 'Built assets/icon.ico from assets/icon.png'
