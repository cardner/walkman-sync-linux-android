$ErrorActionPreference = 'Continue'
$shell = New-Object -ComObject Shell.Application
$ns = $shell.NameSpace(0x11)
$dev = $null
foreach ($i in $ns.Items()) { if ($i.Name -eq 'WALKMAN') { $dev = $i; break } }
if (-not $dev) { Write-Output 'NO DEVICE'; exit 1 }
$folder = $dev.GetFolder
Write-Output '--- DEVICE DETAILS ---'
0..30 | ForEach-Object { $h = $folder.GetDetailsOf($null, $_); if ($h) { Write-Output ("HDR $_=$h") } }
foreach ($s in $folder.Items()) {
  Write-Output ("STORAGE " + $s.Name + " folder=" + $s.IsFolder + " size=" + $s.Size)
  foreach ($prop in @(
    'System.Capacity','System.FreeSpace','System.Size','System.Computer.TotalPhysicalMemory',
    'System.Volume.Capacity','System.Volume.FileSystem','System.PercentFull',
    'System.CapacityUsed','System.CapacityRemaining','System.TotalSize',
    'System.ItemType','System.FileName','System.Devices.StorageCapacity',
    'System.Devices.StorageFreeSpace','System.StorageProviderId'
  )) {
    $v = $null
    try { $v = $s.ExtendedProperty($prop) } catch {}
    if ($null -ne $v -and $v -ne '') { Write-Output ("  PROP $prop=$v") }
  }
  foreach ($idx in 0..30) {
    $v = $folder.GetDetailsOf($s, $idx)
    if ($v) { Write-Output ("  D$idx=" + $v) }
  }
  $sf = $s.GetFolder
  if ($sf) {
    Write-Output ("  --- inner folder headers ---")
    0..20 | ForEach-Object { $h = $sf.GetDetailsOf($null, $_); if ($h) { Write-Output ("  INNERHDR $_=$h") } }
  }
}
