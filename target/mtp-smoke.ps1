$ErrorActionPreference = 'Continue'
$shell = New-Object -ComObject Shell.Application
$ns = $shell.NameSpace(0x11)
$dev = $null
foreach ($i in $ns.Items()) { if ($i.Name -eq 'WALKMAN') { $dev = $i; break } }
$sd = $null
foreach ($s in $dev.GetFolder.Items()) { if ($s.Name -eq 'SlvrWalkman') { $sd = $s; break } }
$root = $sd.GetFolder
foreach ($c in $root.Items()) { if ($c.Name -eq 'Music') { $root = $c.GetFolder; break } }
$artist = $null
foreach ($a in $root.Items()) { if ($a.IsFolder) { $artist = $a; break } }
$album = $null
foreach ($a in $artist.GetFolder.Items()) { if ($a.IsFolder) { $album = $a; break } }
$folder = $album.GetFolder
Write-Output ("ALBUM " + $album.Name)
0..20 | ForEach-Object { Write-Output ("HDR $_=" + $folder.GetDetailsOf($null, $_)) }
$n = 0
foreach ($f in $folder.Items()) {
  if ($f.IsFolder) { continue }
  $extProp = ''
  try { $extProp = [string]$f.ExtendedProperty('System.FileExtension') } catch {}
  $itemType = ''
  try { $itemType = [string]$f.ExtendedProperty('System.ItemType') } catch {}
  $fileExt = ''
  try { $fileExt = [string]$f.ExtendedProperty('System.FileName') } catch {}
  $parsed = ''
  try { $parsed = [string]$f.ExtendedProperty('System.ParsingName') } catch {}
  $path = ''
  try { $path = [string]$f.Path } catch {}
  Write-Output ("NAME=" + $f.Name)
  Write-Output ("TYPE=" + $f.Type)
  Write-Output ("SIZE=" + $f.Size)
  Write-Output ("EXTPROP=" + $extProp)
  Write-Output ("ITEMTYPE=" + $itemType)
  Write-Output ("FILENAME=" + $fileExt)
  Write-Output ("PARSING=" + $parsed)
  Write-Output ("PATH=" + $path)
  foreach ($idx in 0,1,2,3,4,5,6,7,8,9,10,11,12,154,155,156,164,165,166) {
    Write-Output ("D$idx=" + $folder.GetDetailsOf($f, $idx))
  }
  $n++
  if ($n -ge 2) { break }
}
