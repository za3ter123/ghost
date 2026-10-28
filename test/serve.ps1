# Tiny local HTTP server for the web gates (cookies need http; file:// pages get none): serves files from this
# folder on 127.0.0.1:<port> until the parent test process exits. Usage: Start-Job -FilePath serve.ps1 -ArgumentList port, dir, parentPid
param([int]$Port, [string]$Dir, [int]$Parent)
$l = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, $Port); $l.Start()
while ($true) {
  if (-not (Get-Process -Id $Parent -ErrorAction SilentlyContinue)) { break }
  if (-not $l.Pending()) { Start-Sleep -Milliseconds 20; continue }
  $c = $l.AcceptTcpClient(); $s = $c.GetStream(); $r = [IO.StreamReader]::new($s)
  $line = $r.ReadLine(); while (($h = $r.ReadLine()) -ne '' -and $null -ne $h) { }
  $path = ([regex]::Match("$line", '^GET /([^ ?]*)')).Groups[1].Value
  $f = Join-Path $Dir ([IO.Path]::GetFileName($path))
  if ($path -and (Test-Path $f -PathType Leaf)) { $body = [IO.File]::ReadAllBytes($f); $head = "HTTP/1.1 200 OK`r`nContent-Type: text/html; charset=utf-8" }
  else { $body = [Text.Encoding]::UTF8.GetBytes('not found'); $head = 'HTTP/1.1 404 Not Found' }
  $hb = [Text.Encoding]::ASCII.GetBytes("$head`r`nContent-Length: $($body.Length)`r`nConnection: close`r`n`r`n")
  $s.Write($hb, 0, $hb.Length); $s.Write($body, 0, $body.Length); $s.Flush(); $c.Close()
}
$l.Stop()
