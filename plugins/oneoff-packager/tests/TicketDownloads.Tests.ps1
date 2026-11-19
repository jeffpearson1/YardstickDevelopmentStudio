#Requires -Version 7.0
#Requires -Modules @{ ModuleName = 'Pester'; ModuleVersion = '5.0' }
# Installers linked from the ticket: downloaded, described, and staged in the software dropbox.

BeforeAll {
    . (Join-Path $PSScriptRoot 'TestSupport.ps1')
    Import-Module (Join-Path (Get-PluginRoot) 'scripts\OneOffPackager.psm1') -Force

    function Start-TestWebServer {
        # A tiny HTTP server on 127.0.0.1 serving $Routes: path -> @{ Type; Body (bytes); Disposition; Status }.
        param([hashtable]$Routes)
        $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
        $listener.Start()
        $job = Start-ThreadJob -ArgumentList $listener, $Routes -ScriptBlock {
            param($listener, $routes)
            while ($true) {
                try { $client = $listener.AcceptTcpClient() } catch { break }
                try {
                    $stream = $client.GetStream()
                    $reader = [IO.StreamReader]::new($stream, [Text.Encoding]::ASCII, $false, 1024, $true)
                    $line = $reader.ReadLine()
                    while ($reader.ReadLine()) { }
                    $path = ($line -split ' ')[1]
                    $route = $routes[($path -split '\?')[0]]
                    $query = if ($path -match '\?(.*)$') { $Matches[1] } else { '' }
                    if (-not $route) { $route = @{ Status = '404 Not Found'; Type = 'text/plain'; Body = [Text.Encoding]::ASCII.GetBytes('missing') } }
                    if ($route.ContainsKey('NeedsQuery') -and $query -notmatch $route.NeedsQuery) {
                        $route = @{ Status = '200 OK'; Type = 'text/html'; Body = [Text.Encoding]::ASCII.GetBytes('<html>preview page</html>') }
                    }
                    $status = if ($route.ContainsKey('Status')) { $route.Status } else { '200 OK' }
                    $head = "HTTP/1.1 $status`r`nContent-Type: $($route.Type)`r`nContent-Length: $($route.Body.Length)`r`nConnection: close`r`n"
                    if ($route.ContainsKey('Disposition')) { $head += "Content-Disposition: $($route.Disposition)`r`n" }
                    $bytes = [Text.Encoding]::ASCII.GetBytes("$head`r`n")
                    $stream.Write($bytes, 0, $bytes.Length)
                    $stream.Write($route.Body, 0, $route.Body.Length)
                    $stream.Flush()
                } finally { $client.Close() }
            }
        }
        [pscustomobject]@{ Listener = $listener; Job = $job; Base = "http://127.0.0.1:$($listener.LocalEndpoint.Port)" }
    }

    function Stop-TestWebServer { param($Server) $Server.Listener.Stop(); $Server.Job | Wait-Job -Timeout 5 | Out-Null; $Server.Job | Remove-Job -Force }

    function New-TestZip {
        param([hashtable]$Entries)
        $stream = [IO.MemoryStream]::new()
        $zip = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
        foreach ($name in $Entries.Keys) {
            $entry = $zip.CreateEntry($name)
            $writer = $entry.Open(); $writer.Write($Entries[$name], 0, $Entries[$name].Length); $writer.Dispose()
        }
        $zip.Dispose()
        $stream.ToArray()
    }
}

Describe 'ticket links' {
    It 'never writes a query string down' {
        Remove-OneOffUrlQuery 'https://www.dropbox.com/s/abc/Zoom.msi?rlkey=secret&dl=0' | Should -Be 'https://www.dropbox.com/s/abc/Zoom.msi (query removed)'
        Remove-OneOffUrlQuery 'https://example.com/setup.exe' | Should -Be 'https://example.com/setup.exe'
    }

    It 'asks Dropbox and SharePoint for the file instead of the preview page' {
        Get-OneOffDownloadUri 'https://www.dropbox.com/s/abc/Zoom.msi?rlkey=k&dl=0' | Should -Be 'https://www.dropbox.com/s/abc/Zoom.msi?rlkey=k&dl=1'
        Get-OneOffDownloadUri 'https://www.dropbox.com/scl/fo/abc' | Should -Be 'https://www.dropbox.com/scl/fo/abc?dl=1'
        Get-OneOffDownloadUri 'https://contoso.sharepoint.com/:u:/s/IT/abc?e=x1' | Should -Be 'https://contoso.sharepoint.com/:u:/s/IT/abc?e=x1&download=1'
        Get-OneOffDownloadUri 'https://vendor.example/files/setup.exe?v=2' | Should -Be 'https://vendor.example/files/setup.exe?v=2'
    }

    It 'knows other file-sharing services, not only Dropbox' {
        Get-OneOffDownloadUri 'https://drive.google.com/file/d/1AbC_d-9/view?usp=sharing' | Should -Be 'https://drive.usercontent.google.com/download?id=1AbC_d-9&export=download&confirm=t'
        Get-OneOffDownloadUri 'https://drive.google.com/open?id=1AbC' | Should -Be 'https://drive.usercontent.google.com/download?id=1AbC&export=download&confirm=t'
        Get-OneOffDownloadUri 'https://asu.app.box.com/s/k3y9' | Should -Be 'https://asu.app.box.com/shared/static/k3y9'
        Get-OneOffDownloadUri 'https://github.com/o/r/blob/main/setup.msi' | Should -Be 'https://github.com/o/r/blob/main/setup.msi?raw=1'
        Get-OneOffDownloadUri 'https://onedrive.live.com/redir?resid=1' | Should -Be 'https://onedrive.live.com/redir?resid=1&download=1'
        Get-OneOffDownloadUri 'https://wetransfer.com/downloads/abc' | Should -Be 'https://wetransfer.com/downloads/abc'
        foreach ($pair in @(
            @('https://www.dropbox.com/s/a', 'Dropbox'), @('https://contoso-my.sharepoint.com/x', 'OneDrive'), @('https://1drv.ms/u/s!x', 'OneDrive'),
            @('https://drive.google.com/x', 'Google Drive'), @('https://app.box.com/s/x', 'Box'), @('https://we.tl/t-x', 'WeTransfer'),
            @('https://mega.nz/file/x', 'MEGA'), @('https://www.mediafire.com/file/x', 'MediaFire'), @('https://vendor.example/x', ''))) {
            Get-OneOffFileShareHost $pair[0] | Should -Be $pair[1] -Because $pair[0]
        }
    }
}

Describe 'downloading the installers a ticket links to' {
    BeforeEach {
        $t = New-TestWorkItem
        $exe = [IO.File]::ReadAllBytes((Get-Process -Id $PID).Path)
        $zip = New-TestZip @{ 'Tool/tool-setup.exe' = $exe; 'Tool/readme.txt' = [Text.Encoding]::ASCII.GetBytes('read me'); 'Tool/picture.png' = [byte[]](1, 2) }
        $server = Start-TestWebServer @{
            '/files/app-setup.exe' = @{ Type = 'application/octet-stream'; Body = $exe }
            '/download' = @{ Type = 'application/octet-stream'; Body = $exe; Disposition = 'attachment; filename="Vendor Setup 2.1.exe"' }
            '/s/abc/Bundle.zip' = @{ Type = 'application/zip'; Body = $zip; NeedsQuery = 'dl=1' }
            '/product' = @{ Type = 'text/html; charset=utf-8'; Body = [Text.Encoding]::ASCII.GetBytes('<html>product page</html>') }
        }
    }
    AfterEach { Stop-TestWebServer $server; Remove-TestWorkItem $t }

    It 'keeps files, unpacks zips, skips pages and records each file without running it' {
        # The zip is behind a dropbox-style link that only serves the file with dl=1.
        Mock -ModuleName OneOffPackager Get-OneOffDownloadUri { param($Url) if ($Url -like '*Bundle.zip*') { "$($Url -replace '\?.*$', '')?dl=1" } else { $Url } }
        $urls = @("$($server.Base)/files/app-setup.exe", "$($server.Base)/download?token=secret", "$($server.Base)/s/abc/Bundle.zip?dl=0", "$($server.Base)/product", "$($server.Base)/gone")
        $r = Save-OneOffTicketDownloads -WorkItem $t.WorkItem -Urls $urls
        $names = @($r.Files | ForEach-Object { $_.file }) | Sort-Object
        $names | Should -Be @('downloads\app-setup.exe', 'downloads\Bundle\Tool\readme.txt', 'downloads\Bundle\Tool\tool-setup.exe', 'downloads\Vendor Setup 2.1.exe')
        $setup = $r.Files | Where-Object { $_.name -eq 'app-setup.exe' }
        $setup.sha256 | Should -Be ((Get-FileHash -LiteralPath (Get-Process -Id $PID).Path).Hash.ToLowerInvariant())
        $setup.signature | Should -Be 'Valid'
        $setup.signer | Should -Match 'Microsoft'
        $setup.productName | Should -Match 'PowerShell'
        ($r.Links | Where-Object { $_.link -like '*/product' }).result | Should -Match 'web page'
        ($r.Links | Where-Object { $_.link -like '*/gone' }).result | Should -Match 'failed \(HTTP 404\)'
        ($r.Links | Where-Object { $_.link -like '*Bundle.zip*' }).result | Should -Match 'zip unpacked'
        Test-Path (Join-Path $t.WorkItem 'downloads\product') | Should -BeFalse
        $json = Get-Content (Join-Path $t.WorkItem 'downloads.json') -Raw
        $json | Should -Not -Match 'secret'
        $json | Should -Match '/download \(query removed\)'
        (Get-Content (Join-Path $t.WorkItem 'downloads.md') -Raw) | Should -Match 'signer: '
    }

    It 'runs as the fetch-links step and warns about links it could not fetch' {
        Set-TestJson -Path (Join-Path $t.WorkItem 'parsed-ticket.json') -Value @{ Urls = @("$($server.Base)/files/app-setup.exe", "$($server.Base)/gone") }
        $r = Invoke-TestStep -Item $t -Script 'Step-FetchLinks.ps1'
        $r.ExitCode | Should -Be 0 -Because $r.Output
        $r.Result.status | Should -Be 'warn'
        $r.Result.summary | Should -Be '1 file(s) from 2 link(s); 1 link(s) could not be fetched (see downloads.md).'
        @($r.Result.artifacts.path) | Should -Be @('downloads.json', 'downloads.md')
    }

    It 'refuses a zip that would unpack outside its folder' {
        $bad = New-TestZip @{ '../escape.exe' = [byte[]](1, 2, 3) }
        Stop-TestWebServer $server
        $server = Start-TestWebServer @{ '/bad.zip' = @{ Type = 'application/zip'; Body = $bad } }
        $r = Save-OneOffTicketDownloads -WorkItem $t.WorkItem -Urls @("$($server.Base)/bad.zip")
        $r.Links[0].result | Should -Match 'zip kept but not unpacked: it contains an entry outside its folder'
        Test-Path (Join-Path $t.WorkItem 'escape.exe') | Should -BeFalse
    }
}

Describe 'putting ticket installers in the software dropbox' {
    BeforeEach {
        $t = New-TestWorkItem
        $dev = Join-Path $t.Root 'YardstickDev'
        $dropbox = Join-Path $t.Root 'Dropbox'
        New-Item -ItemType Directory -Path (Join-Path $dev 'Local'), (Join-Path $t.WorkItem 'downloads\Bundle') -Force | Out-Null
        Set-Content -LiteralPath (Join-Path $dev 'Local\preferences.yaml') -Value "developmentOnly: true`nSoftwareDropbox: '$dropbox'" -Encoding utf8NoBOM
        Set-Content -LiteralPath (Join-Path $t.WorkItem 'downloads\zoom.msi') -Value 'zoom installer' -NoNewline
        Set-Content -LiteralPath (Join-Path $t.WorkItem 'downloads\Bundle\tool.exe') -Value 'tool installer' -NoNewline
        Set-TestJson -Path (Join-Path $t.WorkItem 'downloads.json') -Value @{ files = @(@{ file = 'downloads\zoom.msi'; productVersion = '6.1'; signer = 'Zoom Video Communications, Inc.' }) }
        Set-TestJson -Path (Join-Path $t.WorkItem 'products.json') -Value @{
            products = @(
                @{ id = 'zoom'; name = 'Zoom'; downloads = @('downloads\zoom.msi') }
                @{ id = 'tool'; name = 'Tool'; downloads = @('downloads\Bundle\tool.exe', 'ticket.md') }
                @{ id = 'other'; name = 'Other' }
            )
            routedProducts = @(@{ id = 'kept' })
        }
        Set-Content -LiteralPath (Join-Path $t.WorkItem 'ticket.md') -Value 'request'
    }
    AfterEach { Remove-TestWorkItem $t }

    It 'copies each product''s downloads under its id and notes them for recipe authoring' {
        $r = Invoke-TestStep -Item $t -Script 'Step-StageDropbox.ps1' -Settings @{ yardstickDevPath = $dev }
        $r.ExitCode | Should -Be 0 -Because $r.Output
        Get-Content (Join-Path $dropbox 'zoom\zoom.msi') -Raw | Should -Be 'zoom installer'
        Get-Content (Join-Path $dropbox 'tool\tool.exe') -Raw | Should -Be 'tool installer'
        Test-Path (Join-Path $dropbox 'tool\ticket.md') | Should -BeFalse -Because 'only files in downloads\ are staged'
        $r.Result.status | Should -Be 'warn'
        $r.Result.summary | Should -Match '^Staged 2 file\(s\) in the software dropbox\. tool: ticket\.md is not a file in downloads'
        $products = Get-Content (Join-Path $t.WorkItem 'products.json') -Raw | ConvertFrom-Json
        $zoom = $products.products | Where-Object id -eq 'zoom'
        $zoom.ticketInstallers[0].stagedAt | Should -Be '<SoftwareDropbox>\zoom\zoom.msi'
        $zoom.ticketInstallers[0].productVersion | Should -Be '6.1'
        $products.routedProducts[0].id | Should -Be 'kept'
        ($products.products | Where-Object id -eq 'other').PSObject.Properties['ticketInstallers'] | Should -BeNullOrEmpty

        # Again: the same files are already there, and a different file is never replaced.
        Set-Content -LiteralPath (Join-Path $dropbox 'tool\tool.exe') -Value 'someone else''s file' -NoNewline
        $r = Invoke-TestStep -Item $t -Script 'Step-StageDropbox.ps1' -Settings @{ yardstickDevPath = $dev }
        $r.Result.summary | Should -Match 'Staged 0 file\(s\) in the software dropbox; 1 were already there\.'
        $r.Result.summary | Should -Match 'tool\\tool\.exe already exists with different contents'
        Get-Content (Join-Path $dropbox 'tool\tool.exe') -Raw | Should -Be 'someone else''s file'
    }

    It 'keeps the downloads in the ticket when no SoftwareDropbox is configured' {
        Remove-Item -LiteralPath (Join-Path $dev 'Local\preferences.yaml')
        $r = Invoke-TestStep -Item $t -Script 'Step-StageDropbox.ps1' -Settings @{ yardstickDevPath = $dev }
        $r.ExitCode | Should -Be 0 -Because $r.Output
        $r.Result.status | Should -Be 'warn'
        $r.Result.summary | Should -Match '^No SoftwareDropbox is set'
        Test-Path $dropbox | Should -BeFalse
        $zoom = (Get-Content (Join-Path $t.WorkItem 'products.json') -Raw | ConvertFrom-Json).products | Where-Object id -eq 'zoom'
        $zoom.ticketInstallers[0].stagedAt | Should -BeNullOrEmpty
    }
}
