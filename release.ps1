# Публикува нова версия в GitHub Releases, откъдето колегите я получават с бутона „Обнови“.
# Преди това вдигни version в Cargo.toml (напр. 0.2.0 → 0.3.0) и запиши промените в git.
#   .\release.ps1 -Notes "Какво е новото"
#   .\release.ps1 -Notes "..." -Full    # + пълен пакет (exe + bin) за първоначална инсталация
param([string]$Notes = "", [switch]$Full)
# Без $ErrorActionPreference = 'Stop': cargo, git и gh пишат прогрес в stderr и PowerShell 5.1 би спрял.
Set-Location $PSScriptRoot

$mingw = "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin"
$env:PATH = "$env:USERPROFILE\.cargo\bin;$mingw;C:\Program Files\GitHub CLI;$env:PATH"

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$tag = "v$version"
gh release view $tag *> $null
if ($LASTEXITCODE -eq 0) { throw "Версия $tag вече е публикувана — вдигни version в Cargo.toml." }
if (git status --porcelain) { throw "Има незаписани промени — направи git commit преди release." }

cargo build --release
if ($LASTEXITCODE) { throw "Компилацията не успя." }
try { Copy-Item target\release\pp_downloader.exe PP_Downloader.exe -Force -ErrorAction Stop } catch { throw "PP_Downloader.exe не може да се замени (програмата е пусната от тази папка?) — затвори я и пусни пак." }

$assets = @("PP_Downloader.exe")
if ($Full) {
    $zip = "PP_Downloader_full.zip"
    Remove-Item $zip -ErrorAction SilentlyContinue
    Compress-Archive -Path PP_Downloader.exe, bin, README.md -DestinationPath $zip
    $assets += $zip
}

git push -u origin HEAD
if ($LASTEXITCODE) { throw "git push не успя." }
gh release create $tag @assets --title "PP Downloader $tag" --notes $Notes
if ($LASTEXITCODE) { throw "Публикуването не успя." }
if ($Full) { Remove-Item $zip }
Write-Host "Публикувана е $tag" -ForegroundColor Green
