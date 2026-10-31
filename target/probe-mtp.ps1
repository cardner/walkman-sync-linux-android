$shell = New-Object -ComObject Shell.Application
$ns = $shell.NameSpace(0x11)
if (-not $ns) {
    Write-Output "NO_NAMESPACE"
    exit
}
Write-Output "=== This PC items ==="
foreach ($i in $ns.Items()) {
    $n = $i.Name
    $stor = @()
    try {
        foreach ($s in $i.GetFolder.Items()) {
            $stor += $s.Name
        }
    } catch {}
    Write-Output ("ITEM`t" + $n + "`t" + ($stor -join "|"))
}

Write-Output "=== PnP portable ==="
Get-PnpDevice -PresentOnly | Where-Object {
    $_.FriendlyName -match 'WALKMAN|NW-|Sony|MTP|Portable|ZX|WM1|A10|A30' -or
    $_.Class -match 'WPD|Android|USBDevice'
} | Select-Object Status, Class, FriendlyName, InstanceId | Format-Table -AutoSize | Out-String
