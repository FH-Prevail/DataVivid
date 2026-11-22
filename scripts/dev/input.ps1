param([int]$ProcId, [string]$Action, [int]$X = 0, [int]$Y = 0, [string]$Keys = '')
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class I {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
"@
[I]::SetProcessDPIAware() | Out-Null
$h = (Get-Process -Id $ProcId).MainWindowHandle
[I]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 250
$r = New-Object I+RECT
[I]::GetWindowRect($h, [ref]$r) | Out-Null
$sx = $r.L + $X; $sy = $r.T + $Y
switch ($Action) {
  'click'  { [I]::SetCursorPos($sx, $sy); Start-Sleep -Milliseconds 120; [I]::mouse_event(2,0,0,0,[IntPtr]::Zero); Start-Sleep -Milliseconds 60; [I]::mouse_event(4,0,0,0,[IntPtr]::Zero) }
  'dbl'    { [I]::SetCursorPos($sx, $sy); Start-Sleep -Milliseconds 120; 1..2 | ForEach-Object { [I]::mouse_event(2,0,0,0,[IntPtr]::Zero); Start-Sleep -Milliseconds 40; [I]::mouse_event(4,0,0,0,[IntPtr]::Zero); Start-Sleep -Milliseconds 60 } }
  'rclick' { [I]::SetCursorPos($sx, $sy); Start-Sleep -Milliseconds 120; [I]::mouse_event(8,0,0,0,[IntPtr]::Zero); Start-Sleep -Milliseconds 60; [I]::mouse_event(16,0,0,0,[IntPtr]::Zero) }
  'move'   { [I]::SetCursorPos($sx, $sy) }
  'keys'   { [System.Windows.Forms.SendKeys]::SendWait($Keys) }
  'wheel'  { [I]::SetCursorPos($sx, $sy); Start-Sleep -Milliseconds 100; [I]::mouse_event(0x0800,0,0,[uint32]([int]$Keys -band 0xffffffff),[IntPtr]::Zero) }
}
Start-Sleep -Milliseconds 500
