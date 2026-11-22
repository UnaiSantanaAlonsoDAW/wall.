$ErrorActionPreference = 'Stop'
$projectRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Push-Location $projectRoot
try {
    $distRoot = Join-Path $projectRoot 'dist'
    New-Item -ItemType Directory -Path $distRoot -Force | Out-Null
    $exe = Join-Path $distRoot 'wall.exe'
    if (!(Test-Path -LiteralPath $exe) -or
        (Get-FileHash -LiteralPath $exe).Hash -ne (Get-FileHash -LiteralPath 'target\release\wall.exe').Hash) {
        Copy-Item -LiteralPath 'target\release\wall.exe' -Destination $exe
    }
    foreach ($name in @('README.md', 'README.es.md', 'LICENSE', 'VALIDATION.md')) {
        Copy-Item -LiteralPath (Join-Path $projectRoot $name) -Destination (Join-Path $distRoot $name)
    }
    $docsRoot = Join-Path $distRoot 'docs'
    New-Item -ItemType Directory -Path $docsRoot -Force | Out-Null
    foreach ($name in @('carousel.png', 'gallery-demo.jpg', 'glass-edges.png')) {
        Copy-Item -LiteralPath (Join-Path $projectRoot "docs\$name") -Destination (Join-Path $docsRoot $name)
    }
    # Bundle the license files supplied by resolved Windows dependencies and fonts.
    $metadata = cargo metadata --locked --offline --format-version 1 --filter-platform x86_64-pc-windows-msvc | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed' }
    $resolved = @{}
    foreach ($node in $metadata.resolve.nodes) { $resolved[$node.id] = $true }
    $licensesRoot = Join-Path $distRoot 'licenses'
    New-Item -ItemType Directory -Path $licensesRoot -Force | Out-Null
    $notices = @('# Third-party components', '', 'Public crates compiled for Windows; original bundled license files are in licenses/.', '')
    foreach ($package in ($metadata.packages | Sort-Object name)) {
        if (-not $resolved.ContainsKey($package.id) -or $package.name -eq 'wall') { continue }
        $notices += ('- {0} {1}: {2}. {3}' -f $package.name, $package.version, $package.license, $package.repository)
        $sourceRoot = Split-Path -Parent $package.manifest_path
        $licenseFiles = @(Get-ChildItem -LiteralPath $sourceRoot -File | Where-Object { $_.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE)' })
        if ($package.name -eq 'epaint_default_fonts') {
            $licenseFiles += @(Get-ChildItem -LiteralPath (Join-Path $sourceRoot 'fonts') -File -Filter '*.txt')
        }
        if ($licenseFiles.Count -gt 0) {
            $packageRoot = Join-Path $licensesRoot ($package.name + '-' + $package.version)
            New-Item -ItemType Directory -Path $packageRoot -Force | Out-Null
            foreach ($file in $licenseFiles) { Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $packageRoot $file.Name) }
        }
    }
    $notices | Set-Content -LiteralPath (Join-Path $distRoot 'THIRD_PARTY_NOTICES.md') -Encoding utf8
    $version = ($metadata.packages | Where-Object { $_.name -eq 'wall' }).version
    $archive = Join-Path $distRoot ('wall-' + $version + '-windows-x64.zip')
    Compress-Archive -Path $exe, (Join-Path $distRoot 'README.md'), (Join-Path $distRoot 'README.es.md'), (Join-Path $distRoot 'LICENSE'), (Join-Path $distRoot 'VALIDATION.md'), (Join-Path $distRoot 'THIRD_PARTY_NOTICES.md'), $licensesRoot, $docsRoot -DestinationPath $archive -Force
    Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $distRoot 'wall.exe'), $archive
} finally { Pop-Location }
