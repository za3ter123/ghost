// Foreground watcher for ghost tests, in its own process. Stretch durations come from a 1 ms GetForegroundWindow
// poll on a time-critical thread that does nothing slow (P lines, timed by a high-resolution clock). A WinEvent hook
// on EVENT_SYSTEM_FOREGROUND is logged alongside as a cross-check only (E lines): its event time for a hand-back is
// stamped when the receiving window's thread finishes activating, measured ~100 ms after GetForegroundWindow has
// already moved (2026-10-17: 218 ms by the hook against 0 ms by the poll). Ownership lookups (WMI) run on a writer
// thread, so they can't stall the poll. Lines, until the stop file appears or the test (arg 3) exits:
//   <ms> <E|P> <hwnd> <class> <P parked off screen|V> <pid> <N process started after the watcher|O> <L in ghost's launched.txt|-> <tick> <G ghost's|F foreign> <process> <title...>
// <ms> is the high-resolution clock when the poll saw the change (P) or the hook callback ran (E); <tick> is
// GetTickCount at the poll (P) or the event's own time (E). The ready file appears once the first poll sample lands.
// Just before END: `<ms> GHOST <hwnd>...`, every hwnd seen in ghost's launched.txt or a park file during the run, so
// the summary can call a window ghost's by hwnd even in a shared host process (explorer.exe, ApplicationFrameHost)
// and even when ghost recorded it only after it took the foreground.
using System; using System.IO; using System.Text; using System.Diagnostics; using System.Runtime.InteropServices; using System.Threading;
using System.Collections.Concurrent;
class FgWatch {
  delegate void WinEventProc(IntPtr hook, uint ev, IntPtr hwnd, int obj, int child, uint thread, uint time);
  [DllImport("user32.dll")] static extern IntPtr SetWinEventHook(uint min, uint max, IntPtr mod, WinEventProc proc, uint pid, uint tid, uint flags);
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] static extern uint MsgWaitForMultipleObjects(uint n, IntPtr[] h, bool all, uint ms, uint mask);
  [StructLayout(LayoutKind.Sequential)] struct MSG { public IntPtr h; public uint m; public IntPtr w, l; public uint t; public int x, y; }
  [DllImport("user32.dll")] static extern bool PeekMessage(out MSG m, IntPtr h, uint a, uint b, uint r);
  [DllImport("user32.dll")] static extern bool TranslateMessage(ref MSG m);
  [DllImport("user32.dll")] static extern IntPtr DispatchMessage(ref MSG m);
  [DllImport("kernel32.dll")] static extern uint GetTickCount();
  [DllImport("kernel32.dll")] static extern IntPtr GetCurrentThread();
  [DllImport("kernel32.dll")] static extern bool SetThreadPriority(IntPtr t, int p);
  [DllImport("winmm.dll")] static extern uint timeBeginPeriod(uint p);
  [StructLayout(LayoutKind.Sequential)] struct RC { public int L, T, R, B; }
  [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RC r);
  [DllImport("user32.dll")] static extern IntPtr GetAncestor(IntPtr h, uint f);
  [DllImport("user32.dll")] static extern int GetSystemMetrics(int i);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  static Stopwatch clock = Stopwatch.StartNew();
  static DateTime t0 = DateTime.Now; static string launched;
  static WinEventProc keep;
  // One sample: what was seen and when; everything slow about it is resolved later by the writer.
  class S { public double Ms; public char Src; public IntPtr H; public string Cls; public uint Pid; public uint Tick; }
  static BlockingCollection<S> q = new BlockingCollection<S>();
  static S Take(char src, IntPtr h, uint tick) {
    var c = new StringBuilder(128); GetClassName(h, c, 128);
    uint pid; GetWindowThreadProcessId(h, out pid);
    return new S { Ms = clock.Elapsed.TotalMilliseconds, Src = src, H = h, Cls = c.Length > 0 ? c.ToString() : "-", Pid = pid, Tick = tick };
  }
  // Whose a pid is, cached: "G <name>" when ghost started it (ghost.exe, the fixture, a browser on a ghost throwaway
  // profile, or a child of one of those), else "F <name>". Command lines and parents come from WMI.
  static System.Collections.Generic.Dictionary<uint, string> owner = new System.Collections.Generic.Dictionary<uint, string>();
  static string Owner(uint pid) {
    if (pid == 0) return "F -";
    string o; if (owner.TryGetValue(pid, out o)) return o;
    string name = "-", cmd = ""; uint parent = 0;
    try {
      using (var s = new System.Management.ManagementObjectSearcher("SELECT Name, CommandLine, ParentProcessId FROM Win32_Process WHERE ProcessId=" + pid))
        foreach (System.Management.ManagementObject m in s.Get()) { name = (m["Name"] as string) ?? "-"; cmd = (m["CommandLine"] as string) ?? ""; parent = (uint)m["ParentProcessId"]; }
    } catch { }
    string low = name.ToLowerInvariant();
    bool g = low == "ghost.exe" || low.StartsWith("ghost-fixture") || cmd.IndexOf(@"\ghost\profiles\ghost-", StringComparison.OrdinalIgnoreCase) >= 0;
    owner[pid] = "F " + name; // guard against a parent cycle while recursing
    if (!g && parent != 0 && parent != pid && Owner(parent).StartsWith("G") && !low.Equals("explorer.exe")) g = true;
    o = (g ? "G " : "F ") + name.Replace(' ', '_');
    owner[pid] = o;
    return o;
  }
  static string Vis(IntPtr h) {
    RC r; if (h == IntPtr.Zero || !GetWindowRect(GetAncestor(h, 2), out r)) return "V";
    int l = GetSystemMetrics(76), t = GetSystemMetrics(77), rr = l + GetSystemMetrics(78), b = t + GetSystemMetrics(79);
    return (r.L < rr && r.R > l && r.T < b && r.B > t) ? "V" : "P";
  }
  static string Line(S s) {
    string fresh = "O";
    try { if (s.Pid != 0 && Process.GetProcessById((int)s.Pid).StartTime > t0) fresh = "N"; } catch { }
    string lst = "-";
    try {
      foreach (var line in File.ReadAllLines(launched)) {
        var p = line.Split(' ');
        // By hwnd always; by pid only outside Explorer, whose one process also hosts the owner's Explorer windows.
        if (p.Length >= 2 && (p[0] == s.H.ToInt64().ToString() || (p[1] == s.Pid.ToString() && !Owner(s.Pid).EndsWith(" explorer.exe")))) { lst = "L"; break; }
      }
    } catch { }
    var t = new StringBuilder(80); GetWindowText(s.H, t, 80);
    string who = lst == "L" ? "G " + Owner(s.Pid).Substring(2) : Owner(s.Pid);
    return s.Ms.ToString("F1", System.Globalization.CultureInfo.InvariantCulture) + " " + s.Src + " " + s.H.ToInt64() + " " + s.Cls + " " + Vis(s.H) + " " + s.Pid + " " + fresh + " " + lst + " " + s.Tick + " " + who + " " + t.ToString().Replace('\r', ' ').Replace('\n', ' ');
  }
  // Ghost's windows right now: launched.txt's first column and park-<hwnd>.txt names.
  static void Collect(System.Collections.Generic.HashSet<long> set, string dir) {
    long h;
    try { foreach (var line in File.ReadAllLines(launched)) if (long.TryParse(line.Split(' ')[0], out h)) set.Add(h); } catch { }
    try { foreach (var f in Directory.GetFiles(dir, "park-*.txt")) { var b = Path.GetFileNameWithoutExtension(f).Substring(5); if (long.TryParse(b, out h)) set.Add(h); } } catch { }
  }
  static void Main(string[] a) {
    var w = new StreamWriter(a[0], false) { AutoFlush = true };
    launched = Path.Combine(Environment.GetEnvironmentVariable("LOCALAPPDATA") ?? "", "ghost", "launched.txt");
    timeBeginPeriod(1);
    bool stop = false;
    var poll = new Thread(() => {
      SetThreadPriority(GetCurrentThread(), 15); // THREAD_PRIORITY_TIME_CRITICAL
      IntPtr last = new IntPtr(-1);
      while (!Volatile.Read(ref stop)) {
        var h = GetForegroundWindow();
        if (h != last) {
          bool first = last == new IntPtr(-1);
          last = h; q.Add(Take('P', h, GetTickCount()));
          if (first) File.WriteAllText(a[0] + ".ready", "");
        }
        Thread.Sleep(1);
      }
    });
    var writer = new Thread(() => { foreach (var s in q.GetConsumingEnumerable()) w.WriteLine(Line(s)); });
    keep = (hook, ev, h, obj, child, thread, time) => q.Add(Take('E', h, time));
    if (SetWinEventHook(3, 3, IntPtr.Zero, keep, 0, 0, 0) == IntPtr.Zero) { w.WriteLine("HOOK FAILED"); return; }
    writer.Start(); poll.Start();
    Process parent = null; try { parent = Process.GetProcessById(int.Parse(a[2])); } catch { }
    var ghosts = new System.Collections.Generic.HashSet<long>();
    string state = Path.GetDirectoryName(launched);
    int n = 0;
    while (!File.Exists(a[1]) && (parent == null || !parent.HasExited)) {
      MsgWaitForMultipleObjects(0, null, false, 10, 0x4FF);
      MSG m;
      while (PeekMessage(out m, IntPtr.Zero, 0, 0, 1)) { TranslateMessage(ref m); DispatchMessage(ref m); }
      if (n++ % 5 == 0) Collect(ghosts, state);
    }
    Collect(ghosts, state);
    Volatile.Write(ref stop, true); poll.Join();
    double end = clock.Elapsed.TotalMilliseconds;
    q.CompleteAdding(); writer.Join();
    w.WriteLine(end.ToString("F1", System.Globalization.CultureInfo.InvariantCulture) + " GHOST" + (ghosts.Count > 0 ? " " + string.Join(" ", ghosts) : ""));
    w.WriteLine(end.ToString("F1", System.Globalization.CultureInfo.InvariantCulture) + " END " + GetTickCount());
  }
}
