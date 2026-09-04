<#
  TLToolBox · 验证 exe 内嵌应用图标资源是否生效
  ============================================================
  检查对象：target\release\tltoolbox.exe（先执行 cargo build --release）。

  验证内容（与托盘运行期加载路径一致，纯 P/Invoke 静态读取，不运行 exe）：
    1) RT_GROUP_ICON（类型 14，名 "1"）存在，解析其帧目录并打印帧表
       （尺寸 / 色深 / 指向的 RT_ICON 资源 ID）；
    2) RT_ICON（类型 3）资源存在且数量 ≥ 组目录帧数，组内每个引用 ID 都能
       在 RT_ICON 枚举中命中（组/帧两级的完整性）；
    3) 断言 16 / 32 / 48 / 256 四种尺寸均在场（res/app.ico 的设计规格），
       且 32×32 帧可解码为 32bpp DIB（托盘运行期取用的正是该帧）；
    4) （可选 -Preview）经 ExtractAssociatedIcon 抽出实际图标并放大渲染为
       target\release\app-icon-preview.png 供人工目检。

  用法：
    .\scripts\verify-icon.ps1
    .\scripts\verify-icon.ps1 -ExePath .\target\release\tltoolbox.exe -Preview
#>
param(
    [string]$ExePath = (Join-Path $PSScriptRoot '..\target\release\tltoolbox.exe'),
    [switch]$Preview
)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path $ExePath)) { throw "找不到 exe：$ExePath（先执行 cargo build --release）" }
$exe = (Resolve-Path $ExePath).Path
Write-Host ("目标 exe：{0}" -f $exe)

# ---------- P/Invoke：资源读取 / 枚举 ----------
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Collections.Generic;

public static class IconResReader {
    public delegate bool EnumResNameProc(IntPtr hModule, IntPtr lpszType, IntPtr lpszName, IntPtr lParam);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr LoadLibraryEx(string p, IntPtr h, uint flags);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool FreeLibrary(IntPtr h);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr FindResource(IntPtr h, IntPtr name, IntPtr type);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr LoadResource(IntPtr h, IntPtr res);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern uint SizeofResource(IntPtr h, IntPtr res);
    [DllImport("kernel32.dll")]
    public static extern IntPtr LockResource(IntPtr data);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern bool EnumResourceNames(IntPtr h, IntPtr type, EnumResNameProc cb, IntPtr param);

    public static List<int> EnumerateIds(IntPtr h, int type) {
        var ids = new List<int>();
        EnumResNameProc cb = (hm, t, name, lp) => {
            // 资源名高位为 0 表示数字 ID（MAKEINTRESOURCE），字符串名高位非 0。
            long v = name.ToInt64();
            if ((v >> 16) == 0) { ids.Add((int)(v & 0xFFFF)); }
            return true;
        };
        EnumResourceNames(h, new IntPtr(type), cb, IntPtr.Zero);
        return ids;
    }

    public static byte[] ReadResource(IntPtr h, int name, int type) {
        IntPtr res = FindResource(h, new IntPtr(name), new IntPtr(type));
        if (res == IntPtr.Zero) { return null; }
        uint size = SizeofResource(h, res);
        IntPtr ptr = LockResource(LoadResource(h, res));
        if (ptr == IntPtr.Zero) { return null; }
        byte[] bytes = new byte[size];
        Marshal.Copy(ptr, bytes, 0, (int)size);
        return bytes;
    }
}
"@

# ---------- 读取并断言 ----------
$h = [IconResReader]::LoadLibraryEx($exe, [IntPtr]::Zero, 0x22) # DATAFILE | AS_IMAGE_RESOURCE
if ($h -eq [IntPtr]::Zero) { throw "LoadLibraryEx 失败，无法读取资源表" }
try {
    $allPass = $true
    Write-Host ''
    Write-Host '[1/3] RT_GROUP_ICON（类型 14，名 "1"）帧目录…'

    $group = [IconResReader]::ReadResource($h, 1, 14)
    if (-not $group) { throw '未找到 RT_GROUP_ICON(#1)：应用图标未嵌入！' }
    $count = [BitConverter]::ToUInt16($group, 4)
    Write-Host ("  组图标存在（{0} 帧）。帧表：" -f $count)
    $entries = @()
    for ($i = 0; $i -lt $count; $i++) {
        $o = 6 + $i * 14
        $w = $group[$o];     if ($w -eq 0) { $w = 256 }
        $hgt = $group[$o+1]; if ($hgt -eq 0) { $hgt = 256 }
        $bpp = [BitConverter]::ToUInt16($group, $o + 6)
        $len = [BitConverter]::ToUInt32($group, $o + 8)
        $id  = [BitConverter]::ToUInt16($group, $o + 12)
        $entries += , @{ W = $w; H = $hgt; Bpp = $bpp; Id = $id; Len = $len }
        Write-Host ("    帧 {0}x{1}  {2}bpp  → RT_ICON(#{3})  [{4} 字节]" -f $w, $hgt, $bpp, $id, $len)
    }

    Write-Host ''
    Write-Host '[2/3] RT_ICON（类型 3）资源完整性…'
    $iconIds = [IconResReader]::EnumerateIds($h, 3)
    Write-Host ("  枚举到 RT_ICON 资源 {0} 个：#{1}" -f $iconIds.Count, ($iconIds -join ', #'))
    if ($iconIds.Count -eq 0) { throw '未找到任何 RT_ICON 资源：图标帧未嵌入！' }
    $refAll = $true
    foreach ($e in $entries) {
        if ($iconIds -notcontains $e.Id) {
            Write-Host ("    FAIL：帧目录引用 RT_ICON(#{0}) 缺失！" -f $e.Id)
            $refAll = $false; $allPass = $false
        }
    }
    Write-Host ("  组/帧引用完整性：{0}" -f $(if ($refAll) { 'PASS' } else { 'FAIL' }))

    Write-Host ''
    Write-Host '[3/3] 关键规格断言（res/app.ico 设计规格）…'
    foreach ($want in 16, 32, 48, 256) {
        $hit = $entries | Where-Object { $_.W -eq $want -and $_.H -eq $want }
        $bppOk = $hit -and ($hit | Where-Object { $_.Bpp -eq 32 })
        Write-Host ("  {0}x{1} 帧（32bpp）: {2}" -f $want, $want, $(if ($bppOk) { 'PASS' } else { 'FAIL' }))
        if (-not $bppOk) { $allPass = $false }
    }
    # 托盘运行期加载的是 32×32 帧：直接解码其 DIB 头自证可读。
    $e32 = $entries | Where-Object { $_.W -eq 32 } | Select-Object -First 1
    if ($e32) {
        $dib = [IconResReader]::ReadResource($h, $e32.Id, 3)
        $isDib = $dib.Length -ge 40 -and [BitConverter]::ToUInt32($dib, 0) -eq 40
        $dibOk = $isDib -and [BitConverter]::ToUInt16($dib, 14) -eq 32 -and [BitConverter]::ToUInt32($dib, 16) -eq 0
        Write-Host ("  32×32 帧为 32bpp DIB（托盘解码前置条件）: {0}" -f $(if ($dibOk) { 'PASS' } else { 'FAIL' }))
        if (-not $dibOk) { $allPass = $false }
    }
} finally {
    [IconResReader]::FreeLibrary($h) | Out-Null
}

Write-Host ''
Write-Host ('结果：' + $(if ($allPass) { '全部通过 —— RT_GROUP_ICON(#1) 与 RT_ICON 已生效' } else { '存在缺失，请检查 res/app.ico 与 scripts/generate-app-icon.ps1！' }))

# ---------- 可选：抽出实际图标做放大预览 ----------
if ($Preview) {
    Add-Type -AssemblyName System.Drawing
    $previewPath = Join-Path (Split-Path -Parent $exe) 'app-icon-preview.png'
    $icon = [System.Drawing.Icon]::ExtractAssociatedIcon($exe)
    $small = $icon.ToBitmap()
    $canvasW = 32 + 16 + 256
    $canvas = New-Object System.Drawing.Bitmap($canvasW, 288, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($canvas)
    $g.Clear([System.Drawing.Color]::FromArgb(255, 40, 42, 48))
    $g.DrawImage($small, 0, 16, 32, 32)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor
    $g.DrawImage($small, 48, 16, 256, 256)
    $g.Dispose()
    $canvas.Save($previewPath, [System.Drawing.Imaging.ImageFormat]::Png)
    $canvas.Dispose(); $small.Dispose(); $icon.Dispose()
    Write-Host "放大预览已写出：$previewPath（左：32px 原生；右：8× 放大）"
}

exit $(if ($allPass) { 0 } else { 1 })
