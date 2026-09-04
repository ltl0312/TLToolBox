<#
  TLToolBox · 生成标准 Windows 应用图标 res/app.ico
  ============================================================
  用途：为 build.rs 的 winresource 嵌入分支产出多尺寸合法 .ico
       （RT_GROUP_ICON 由 rc.exe 编译进 exe，替换 Explorer / 任务栏
        默认白板图标；托盘图标运行期也从 exe 资源读取同一份图标）。

  产出：res/app.ico —— 四个帧：
          16×16 / 32×32 / 48×48   32bpp BMP(DIB) 帧（兼容性最佳）
          256×256                 32bpp PNG 帧（体积小、Vista+ 标准）
       （以及 target/icon-zoomed.png 放大预览，供人工检查，不入库）

  设计：暗黑底色圆角方块（近似黑 #10131B → #262C3B 垂直渐变）
       + 白色系 "TL" 字母组合（Segoe UI Bold，白→淡蓝渐变，
       经 GraphicsPath 统一缩放，各尺寸几何一致）
       + 品牌蓝圆头短横线（工具箱“抽屉/底座”点缀）。

  依赖：Windows PowerShell 5.1（System.Drawing / GDI+）。
       PowerShell 7+（pwsh core）不内置 System.Drawing，请用
       powershell.exe 运行本脚本。

  用法：
    powershell -ExecutionPolicy Bypass -File .\scripts\generate-app-icon.ps1
#>
[CmdletBinding()]
param(
    # 输出 .ico 路径（默认：包根 res/app.ico）
    [string]$OutIco = (Join-Path $PSScriptRoot '..\res\app.ico'),
    # 放大预览 PNG 路径（默认：target/icon-zoomed.png，供人工 QA）
    [string]$PreviewPng = (Join-Path $PSScriptRoot '..\target\icon-zoomed.png'),
    # 只渲染预览、不写 .ico（调参用）
    [switch]$PreviewOnly
)

$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -ge 6) {
    throw '本脚本依赖 .NET Framework 的 System.Drawing（GDI+），请改用 Windows PowerShell 5.1（powershell.exe）执行。'
}
Add-Type -AssemblyName System.Drawing

# ---------------------------------------------------------------------------
# 颜色 / 小工具
# ---------------------------------------------------------------------------
function From-Hex([string]$hex) { [System.Drawing.ColorTranslator]::FromHtml($hex) }

function Write-U16([System.IO.MemoryStream]$ms, [int]$v) { $ms.Write([BitConverter]::GetBytes([uint16]$v), 0, 2) }
function Write-U32([System.IO.MemoryStream]$ms, [int]$v) { $ms.Write([BitConverter]::GetBytes([uint32]$v), 0, 4) }

# 圆角矩形路径（r <= w/2 与 h/2）
function New-RoundedRectPath([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $d = $r * 2.0
    if ($d -gt $w) { $d = $w; $r = $w / 2.0 }
    if ($d -gt $h) { $d = $h; $r = $h / 2.0 }
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

# ---------------------------------------------------------------------------
# 设计参数（按边长 S 归一化的比例常量，见每个值的注释）
# ---------------------------------------------------------------------------
#   bg ：圆角方块铺满画布（全出血），圆角半径 ~ 21.5%
#   rim：S>=32 时 1px 细描边，压住暗底在浅色/深色托盘的边界
#   glyph "TL"：字母包络盒约占边长 62%（宽）× 42%（高），光学中心略偏上
#   accent：品牌蓝圆头短横线，位于字母下方 ~ 8.5% 边距处，呼应“工具箱底座”

$BgTop   = From-Hex '#262C3B'   # 背景渐变上（略亮的深蓝灰）
$BgBot   = From-Hex '#10131B'   # 背景渐变下（近黑）
$Rim     = [System.Drawing.Color]::FromArgb(120, 148, 163, 196) # 细描边（半透明白蓝灰）
$GlyphTop = From-Hex '#F7FAFF'  # 字母渐变上（近白）
$GlyphBot = From-Hex '#93B0F2'  # 字母渐变下（淡蓝，呼应品牌 #2D74E8）
$Accent   = From-Hex '#4D9BFF'  # 品牌蓝点缀横线

function New-AppFrame([int]$S) {
    $bmp = New-Object System.Drawing.Bitmap($S, $S, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::Half
    $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
    $g.Clear([System.Drawing.Color]::Transparent)

    # ---- 1) 圆角方块底（垂直渐变：上亮下暗） ----
    $bgRect = New-Object System.Drawing.RectangleF(0.0, 0.0, [float]$S, [float]$S)
    $radius = [Math]::Max(2.0, $S * 0.215)
    $bgPath = New-RoundedRectPath 0.0 0.0 ([float]$S) ([float]$S) ([float]$radius)
    $bgBrush = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
        $bgRect, $BgTop, $BgBot,
        [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)
    $g.FillPath($bgBrush, $bgPath)
    if ($S -ge 32) {
        $rimPen = New-Object System.Drawing.Pen($Rim, 1.0)
        $g.DrawPath($rimPen, $bgPath)
        $rimPen.Dispose()
    }

    # ---- 2) "TL" 字母组合（统一缩放，保证 16..256 几何比例一致） ----
    $family = $null
    try { $family = New-Object System.Drawing.FontFamily('Segoe UI') }
    catch { $family = [System.Drawing.FontFamily]::GenericSansSerif }
    $glyphPath = New-Object System.Drawing.Drawing2D.GraphicsPath
    $glyphPath.AddString(
        'TL', $family,
        [int][System.Drawing.FontStyle]::Bold,
        100.0,
        (New-Object System.Drawing.PointF(0.0, 0.0)),
        [System.Drawing.StringFormat]::GenericTypographic)

    # 字母包络盒：占边长 62% 宽 × 42% 高，中心在 (0.50S, 0.525S)
    $boxW = $S * 0.62
    $boxH = $S * 0.42
    $cx = $S * 0.50
    $cy = $S * 0.525
    $bb = $glyphPath.GetBounds()
    $scale = [Math]::Min($boxW / $bb.Width, $boxH / $bb.Height)
    if ($scale -le 0) { $scale = 0.8 }
    $tx = $cx - ($bb.Width * $scale) / 2.0 - $bb.X * $scale
    $ty = $cy - ($bb.Height * $scale) / 2.0 - $bb.Y * $scale
    $m = New-Object System.Drawing.Drawing2D.Matrix
    $m.Translate([float]$tx, [float]$ty)
    $m.Scale([float]$scale, [float]$scale)
    $glyphPath.Transform($m)

    $glyphBrush = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
        (New-Object System.Drawing.RectangleF(0.0, [float]($S * 0.25), [float]$S, [float]($S * 0.5))),
        $GlyphTop, $GlyphBot,
        [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)
    $g.FillPath($glyphBrush, $glyphPath)

    # ---- 3) 品牌蓝圆头短横线（工具箱底座点缀；S=16 时退化为 1px） ----
    if ($S -ge 16) {
        $lineY = [float]($S * 0.845)
        $lineX0 = [float]($S * 0.235)
        $lineX1 = [float]($S * 0.765)
        $penW = [Math]::Max(1.0, $S * 0.055)
        $accentPen = New-Object System.Drawing.Pen($Accent, [float]$penW)
        $accentPen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
        $accentPen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
        $g.DrawLine($accentPen, $lineX0, $lineY, $lineX1, $lineY)
        $accentPen.Dispose()
    }

    $g.Dispose()
    $glyphBrush.Dispose(); $bgBrush.Dispose(); $glyphPath.Dispose(); $bgPath.Dispose()
    return $bmp
}

# ---------------------------------------------------------------------------
# 帧序列化
# ---------------------------------------------------------------------------
# 32bpp Bitmap -> BMP(DIB) 字节（ICO 标准：BITMAPINFOHEADER + 自底向上 BGRA
# XOR + 1bpp AND 掩码；AND 位 = alpha==0 处为 1，兼容 XP 时代的读取路径，
# Vista+ 直接采用 XOR 的 alpha 通道，半透明抗锯齿像素不受 AND 掩码影响）。
function ConvertTo-DibBytes($bmp) {
    $S = $bmp.Width
    $rect = New-Object System.Drawing.Rectangle(0, 0, $S, $S)
    $data = $bmp.LockBits($rect,
        [System.Drawing.Imaging.ImageLockMode]::ReadOnly,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    try {
        $stride = [Math]::Abs($data.Stride)
        $raw = New-Object byte[] ($stride * $S)
        [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $raw, 0, $raw.Length)
    } finally {
        $bmp.UnlockBits($data)
    }

    $xor = New-Object byte[] ($S * $S * 4)
    for ($row = 0; $row -lt $S; $row++) {
        $src = $row * $stride
        # DIB 自底向上：图像顶行写到最后（S-1-row）行
        $dst = ($S - 1 - $row) * $S * 4
        [Array]::Copy($raw, $src, $xor, $dst, $S * 4)
    }

    $maskStride = [Math]::Ceiling($S / 32.0) * 4
    $mask = New-Object byte[] ($maskStride * $S)
    for ($row = 0; $row -lt $S; $row++) {
        for ($x = 0; $x -lt $S; $x++) {
            $a = $xor[($S - 1 - $row) * $S * 4 + $x * 4 + 3]  # 该像素 alpha（BGRA）
            if ($a -eq 0) {
                $byteIdx = $row * $maskStride + [Math]::Floor($x / 8)
                $mask[$byteIdx] = $mask[$byteIdx] -bor (1 -shl (7 - ($x % 8)))
            }
        }
    }

    $ms = New-Object System.IO.MemoryStream
    Write-U32 $ms 40                                   # biSize
    Write-U32 $ms $S                                   # biWidth
    Write-U32 $ms $S                                   # biHeight（正 = 自底向上）
    Write-U16 $ms 1                                    # biPlanes
    Write-U16 $ms 32                                   # biBitCount
    Write-U32 $ms 0                                    # biCompression = BI_RGB
    Write-U32 $ms ($xor.Length + $mask.Length)         # biSizeImage
    $ms.Write((New-Object byte[] (16)), 0, 16)         # biXPels..biClrImportant = 0
    $ms.Write($xor, 0, $xor.Length)
    $ms.Write($mask, 0, $mask.Length)
    return $ms.ToArray()
}

function ConvertTo-PngBytes($bmp) {
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    return $ms.ToArray()
}

# ---------------------------------------------------------------------------
# 主流程：渲染 4 帧 -> 组 ICO 容器
# ---------------------------------------------------------------------------
$sizes = @(16, 32, 48, 256)
$frames = @()
foreach ($S in $sizes) {
    $bmp = New-AppFrame $S
    if ($S -eq 256) {
        $frames += , @{ Size = $S; Data = (ConvertTo-PngBytes $bmp); Png = $true }
    } else {
        $frames += , @{ Size = $S; Data = (ConvertTo-DibBytes $bmp); Png = $false }
    }
    $bmp.Dispose()
}

if (-not $PreviewOnly) {
    $outDir = Split-Path -Parent $OutIco
    if (-not (Test-Path $outDir)) { New-Item -ItemType Directory -Path $outDir | Out-Null }
    $outFull = [System.IO.Path]::GetFullPath($OutIco)

    $ico = New-Object System.IO.MemoryStream
    Write-U16 $ico 0                                   # reserved
    Write-U16 $ico 1                                   # type = icon
    Write-U16 $ico $frames.Count
    $offset = 6 + 16 * $frames.Count
    $entries = @()
    foreach ($f in $frames) {
        $entries += , @{ F = $f; Offset = $offset }
        $offset += $f.Data.Length
    }
    foreach ($e in $entries) {
        $f = $e.F
        $dim = if ($f.Size -ge 256) { 0 } else { $f.Size }   # 0 = 256
        $ico.WriteByte($dim)                                 # bWidth（1 字节）
        $ico.WriteByte($dim)                                 # bHeight（1 字节）
        $ico.WriteByte(0)                                    # bColorCount
        $ico.WriteByte(0)                                    # bReserved
        Write-U16 $ico 1                                     # wPlanes
        Write-U16 $ico 32                                    # wBitCount
        Write-U32 $ico $f.Data.Length
        Write-U32 $ico $e.Offset
    }
    foreach ($e in $entries) { $ico.Write($e.F.Data, 0, $e.F.Data.Length) }
    [System.IO.File]::WriteAllBytes($outFull, $ico.ToArray())
    $ico.Dispose()

    # 回读自检：解析刚写出的容器并打印帧表
    $read = [System.IO.File]::ReadAllBytes($outFull)
    $count = [BitConverter]::ToUInt16($read, 4)
    Write-Host "已生成 $outFull（帧数: $count）"
    for ($i = 0; $i -lt $count; $i++) {
        $base = 6 + $i * 16
        $w = $read[$base]; if ($w -eq 0) { $w = 256 }
        $h = $read[$base + 1]; if ($h -eq 0) { $h = 256 }
        $bpp = [BitConverter]::ToUInt16($read, $base + 6)
        $len = [BitConverter]::ToUInt32($read, $base + 8)
        $off = [BitConverter]::ToUInt32($read, $base + 12)
        $kind = if ($read[$off] -eq 0x89) { 'PNG' } else { 'DIB(BMP)' }
        Write-Host ("  帧 {0}x{1}  {2}bpp  {3}  ({4} 字节)" -f $w, $h, $bpp, $kind, $len)
    }
}

# ---------------------------------------------------------------------------
# 放大预览（最近邻放大，方便目检四帧几何一致性 / 圆角 / 抗锯齿）
# ---------------------------------------------------------------------------
$z = @{ 16 = 8; 32 = 8; 48 = 8; 256 = 2 }
$cellH = @{ 16 = 16 * 8; 32 = 32 * 8; 48 = 48 * 8; 256 = 256 * 2 }
$totalW = 0; foreach ($S in $sizes) { $totalW += $cellH[$S] + 12 }
$totalH = 256 * 2 + 24
$sheet = New-Object System.Drawing.Bitmap($totalW, $totalH, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$sg = [System.Drawing.Graphics]::FromImage($sheet)
$sg.Clear([System.Drawing.Color]::FromArgb(255, 40, 42, 48))
$sg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor
$sg.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::Half
$x = 8
foreach ($S in $sizes) {
    $bmp = New-AppFrame $S
    $sz = $cellH[$S]
    $sg.DrawImage($bmp, $x, 12, $sz, $sz)
    $sg.DrawString("$S`px", (New-Object System.Drawing.Font('Consolas', 10)),
        [System.Drawing.Brushes]::White, $x, $sz + 16)
    $x += $sz + 12
    $bmp.Dispose()
}
$sg.Dispose()
$sheet.Save($PreviewPng, [System.Drawing.Imaging.ImageFormat]::Png)
$sheet.Dispose()
Write-Host "放大预览已写出：$PreviewPng"
