<#
  TLToolBox · 验证 exe 内嵌应用清单（app.manifest）是否生效
  ================================================================
  三层验证（由浅入深，前两层全自动，第三层可选手动）：

  1) 静态提取（权威）：优先调用 Windows SDK 自带 mt.exe 从
     target\release\tltoolbox.exe 的资源段提取 RT_MANIFEST(#1) —— mt.exe
     与系统激活上下文（Activation Context）加载器使用同一套 XML 解析，
     能成功解析即证明清单格式合法、启动时不会触发 SxS 解析错误；
  2) 静态兜底（无 SDK 机器）：mt.exe 缺失时改用纯 PowerShell P/Invoke
     读取 RT_MANIFEST 资源字节并做关键标记断言；
  3) 运行期 DPI 感知（可选，-LaunchTest）：以 --silent 静默拉起 exe，
     经 shcore!GetProcessDpiAwareness 断言进程处于 Per-Monitor 感知
     （值为 2；Per-Monitor V2 无法用该 API 与 v1 区分——需要精确到
     “Per Monitor v2” 请在任务管理器「详细信息」页添加「DPI 感知」
     列观察，或直接查看本脚本提取的清单文本含 PerMonitorV2）。

  用法：
    .\scripts\verify-manifest.ps1
    .\scripts\verify-manifest.ps1 -ExePath .\target\release\tltoolbox.exe
    .\scripts\verify-manifest.ps1 -LaunchTest        # 追加运行期检测
#>
param(
    [string]$ExePath = (Join-Path $PSScriptRoot '..\target\release\tltoolbox.exe'),
    [switch]$LaunchTest
)

$ErrorActionPreference = 'Stop'
$expected = @(
    'asInvoker',                    # UAC：随启动方权限（不高提，不弹 UAC）
    'uiAccess="false"',             # 非 UI 辅助程序
    'PerMonitorV2, PerMonitor',     # DPI：Per-Monitor V2（含 v1 回退）
    'true/pm',                      # 旧式 dpiAware 兜底（Win8.1）
    'Microsoft.Windows.Common-Controls',  # Common-Controls v6
    '6595b64144ccf1df',             # comctl32 v6 的 publicKeyToken
    '{8e0f7a16-b84b-4c10-b011-8b7c8d9e1e5a}'  # supportedOS Win10/11
)

if (-not (Test-Path $ExePath)) { throw "找不到 exe：$ExePath（先执行 cargo build --release）" }
$exe = (Resolve-Path $ExePath).Path
Write-Host ("目标 exe：{0}" -f $exe)

# ---------- 1) mt.exe 提取（权威解析） ----------
$manifestText = $null
$mt = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\bin' -Recurse -Filter mt.exe -ErrorAction SilentlyContinue |
      Where-Object { $_.FullName -match '\\x64\\' } | Sort-Object FullName -Descending | Select-Object -First 1
if ($mt) {
    $out = Join-Path $env:TEMP 'tltoolbox.verify.manifest'
    if (Test-Path $out) { Remove-Item $out }
    $err = & $mt.FullName -nologo "-inputresource:$exe;#1" "-out:$out" 2>&1 | Out-String
    if (Test-Path $out) {
        Write-Host ('[1/3] mt.exe 提取并解析清单：通过（{0}）' -f $mt.FullName)
        $manifestText = [System.IO.File]::ReadAllText($out, [System.Text.Encoding]::UTF8)
    } else {
        Write-Host ('[1/3] mt.exe 解析失败：{0} —— 清单损坏，应用可能无法启动！' -f $err.Trim())
    }
}

# ---------- 2) 纯 PowerShell 兜底读取（无 mt.exe 时） ----------
if (-not $manifestText) {
    Write-Host '[1/3] 未找到 mt.exe，改用 P/Invoke 读取 RT_MANIFEST 资源…'
    Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class ManiReader {
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
  public static extern IntPtr LoadLibraryEx(string p, IntPtr h, uint f);
  [DllImport("kernel32.dll", SetLastError=true)]
  public static extern IntPtr FindResource(IntPtr m, IntPtr n, IntPtr t);
  [DllImport("kernel32.dll")]
  public static extern IntPtr LoadResource(IntPtr m, IntPtr r);
  [DllImport("kernel32.dll")]
  public static extern uint SizeofResource(IntPtr m, IntPtr r);
  [DllImport("kernel32.dll")]
  public static extern IntPtr LockResource(IntPtr d);
}
"@
    $h = [ManiReader]::LoadLibraryEx($exe, [IntPtr]::Zero, 0x22) # DATAFILE | AS_IMAGE_RESOURCE
    if ($h -eq [IntPtr]::Zero) { throw 'LoadLibraryEx 失败，无法读取资源' }
    $ri = [ManiReader]::FindResource($h, [IntPtr]1, [IntPtr]24)  # #1 = RT_MANIFEST
    if ($ri -eq [IntPtr]::Zero) { throw '未找到 RT_MANIFEST(#1) 资源：清单未嵌入！' }
    $sz = [ManiReader]::SizeofResource($h, $ri)
    $p = [ManiReader]::LockResource([ManiReader]::LoadResource($h, $ri))
    $bytes = New-Object byte[] $sz
    [Runtime.InteropServices.Marshal]::Copy($p, $bytes, 0, $sz)
    $manifestText = [System.Text.Encoding]::UTF8.GetString($bytes)
    Write-Host "[1/3] P/Invoke 读取 RT_MANIFEST(#1)：通过（$sz 字节）"
}

# ---------- 3) 关键标记断言 ----------
if (-not $manifestText) { throw '清单内容为空' }
Write-Host '--- 关键标记断言 ---'
$allPass = $true
foreach ($m in $expected) {
    $ok = $manifestText.Contains($m)
    if (-not $ok) { $allPass = $false }
    Write-Host ("  {0,-42}: {1}" -f $m, $(if ($ok) { 'PASS' } else { 'FAIL' }))
}
Write-Host ('[2/3] 清单标记断言：' + $(if ($allPass) { '全部通过' } else { '存在缺失！' }))
Write-Host ''
Write-Host '手动复核（任选）：'
Write-Host '  1) 任务管理器 → 详细信息 → 右键列头勾选「DPI 感知」→ 运行 tltoolbox.exe，应显示“每显示器(高 DPI) V2”'
Write-Host '  2) sigcheck -m tltoolbox.exe（Sysinternals），或上述提取出的清单文本直接查看'

# ---------- 3) 运行期 DPI 检测（可选） ----------
if ($LaunchTest) {
    Write-Host ''
    Write-Host '[3/3] 运行期 DPI 感知检测…'
    $work = Join-Path $env:TEMP 'tltoolbox-dpi-test'
    New-Item -ItemType Directory -Force -Path $work | Out-Null
    $proc = Start-Process -FilePath $exe -ArgumentList '--silent' -WorkingDirectory $work -PassThru
    Start-Sleep -Milliseconds 1500
    Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class DpiCheck {
  [DllImport("shcore.dll")]
  public static extern int GetProcessDpiAwareness(IntPtr hproc, out int value);
  [DllImport("kernel32.dll")]
  public static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
  [DllImport("kernel32.dll")]
  public static extern bool CloseHandle(IntPtr h);
}
"@
    $hProc = [DpiCheck]::OpenProcess(0x1000, $false, $proc.Id) # PROCESS_QUERY_LIMITED_INFORMATION
    $aware = -1
    if ($hProc -ne [IntPtr]::Zero) {
        $v = 0
        $hr = [DpiCheck]::GetProcessDpiAwareness($hProc, [ref]$v)
        [DpiCheck]::CloseHandle($hProc) | Out-Null
        if ($hr -eq 0) { $aware = $v }
    }
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
    switch ($aware) {
        0 { Write-Host '[3/3] 结果：unaware（系统位图拉伸 → 会发虚）——清单未生效！' }
        1 { Write-Host '[3/3] 结果：System DPI Aware（高分屏仍可能被拉伸）' }
        2 { Write-Host '[3/3] 结果：Per-Monitor DPI Aware —— 通过（v2 细节请以任务管理器列为准）' }
        default { Write-Host ('[3/3] 查询失败（HRESULT 0x{0:X8}），跳过运行期断言' -f $hr) }
    }
}
exit $(if ($allPass) { 0 } else { 1 })
