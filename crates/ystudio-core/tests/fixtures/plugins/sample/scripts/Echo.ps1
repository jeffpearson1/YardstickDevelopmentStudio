param(
    [Parameter(Mandatory)][string]$StudioInput,
    [Parameter(Mandatory)][string]$StudioResult
)
$ErrorActionPreference = 'Stop'
$in = Get-Content -LiteralPath $StudioInput -Raw | ConvertFrom-Json
Set-Content -LiteralPath (Join-Path $in.workItem 'echo.txt') -Value $in.params.message
@{ status = 'ok'; summary = "Echoed $($in.params.message)"; outputs = @{ echo = 'echo.txt' } } |
    ConvertTo-Json | Set-Content -LiteralPath $StudioResult -Encoding utf8
