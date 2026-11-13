# Regression test: all-minimized card count + repeated card clicks
$code = @'
using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Collections.Generic;
using System.Drawing;
using System.Drawing.Imaging;

public class RT {
  [DllImport("user32.dll", CharSet = CharSet.Unicode)]
  public static extern IntPtr FindWindowW(string cls, string title);
  [DllImport("user32.dll")]
  public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")]
  public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")]
  public static extern bool IsIconic(IntPtr h);
  [DllImport("user32.dll")]
  public static extern IntPtr GetWindow(IntPtr h, uint cmd);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)]
  public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")]
  public static extern int GetWindowTextLengthW(IntPtr h);
  [DllImport("user32.dll")]
  public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")]
  public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("dwmapi.dll")]
  public static extern int DwmGetWindowAttribute(IntPtr h, int attr, out int val, int size);
  [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")]
  public static extern long GetWindowLongPtr(IntPtr h, int idx);
  [DllImport("user32.dll")]
  public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")]
  public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, IntPtr extra);
  [DllImport("user32.dll")]
  public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")]
  public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);

  [StructLayout(LayoutKind.Sequential)]
  public struct RECT { public int left, top, right, bottom; }

  static List<IntPtr> TaskbarWindows() {
    var list = new List<IntPtr>();
    EnumWindows((h, l) => {
      if (!IsWindowVisible(h)) return true;
      if (GetWindow(h, 4) != IntPtr.Zero) return true;              // GW_OWNER
      long ex = GetWindowLongPtr(h, -20);
      if ((ex & 0x80) != 0) return true;                             // WS_EX_TOOLWINDOW
      int cloaked = 0;
      DwmGetWindowAttribute(h, 14, out cloaked, 4);
      if (cloaked != 0) return true;
      if (GetWindowTextLengthW(h) == 0) return true;
      string t = Title(h);
      if (t == "GlazeWM Stage" || t.Contains("DWM Notification")) return true;
      list.Add(h);
      return true;
    }, IntPtr.Zero);
    return list;
  }

  static string Title(IntPtr h) {
    var sb = new StringBuilder(200);
    GetWindowTextW(h, sb, sb.Capacity);
    return sb.ToString();
  }

  public static string List() {
    var sb = new StringBuilder();
    foreach (var h in TaskbarWindows()) {
      string flag = (h == GetForegroundWindow() ? "FOCUS " : "") + (IsIconic(h) ? "min" : "VIS");
      sb.AppendLine(h + " [" + flag + "] " + Title(h));
    }
    sb.AppendLine("fg=" + GetForegroundWindow());
    return sb.ToString();
  }

  public static string MinAll() {
    var sb = new StringBuilder();
    foreach (var h in TaskbarWindows()) {
      ShowWindow(h, 6);
      sb.AppendLine("min " + h + " " + Title(h));
    }
    return sb.ToString();
  }

  public static string Shot(string path) {
    IntPtr h = FindWindowW(null, "GlazeWM Stage");
    if (h == IntPtr.Zero) return "no panel";
    RECT r;
    GetWindowRect(h, out r);
    int w = r.right - r.left, hh = r.bottom - r.top;
    using (Bitmap bmp = new Bitmap(w, hh)) {
      using (Graphics g = Graphics.FromImage(bmp)) {
        IntPtr hdc = g.GetHdc();
        PrintWindow(h, hdc, 2);
        g.ReleaseHdc(hdc);
      }
      bmp.Save(path, ImageFormat.Png);
    }
    return "panel " + w + "x" + hh + " at " + r.left + "," + r.top + " -> " + path;
  }

  public static void Click(int x, int y) {
    SetCursorPos(x, y);
    System.Threading.Thread.Sleep(60);
    mouse_event(0x2, 0, 0, 0, IntPtr.Zero);
    mouse_event(0x4, 0, 0, 0, IntPtr.Zero);
  }
}
'@
Add-Type -TypeDefinition $code -ReferencedAssemblies System.Drawing,System.Drawing.Primitives

Write-Host "=== BEFORE (normal) ==="
[RT]::List()
Write-Host "=== MINIMIZE ALL ==="
[RT]::MinAll() | Out-Null
Start-Sleep -Milliseconds 600
Write-Host "=== ALL-MIN LIST ==="
[RT]::List()
Write-Host "=== PANEL SHOT ==="
[RT]::Shot('C:\Code\glazewm-stage-manager\shots\03_allmin_panel.png')
