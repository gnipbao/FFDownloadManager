param(
  [Parameter(Mandatory = $true)][string]$Path,
  [ValidateSet(2, 3)][int]$ExpectedSubsystem = 2
)

$ErrorActionPreference = 'Stop'
$reader = [System.IO.BinaryReader]::new([System.IO.File]::OpenRead((Resolve-Path $Path).Path))
try {
  if ($reader.ReadUInt16() -ne 0x5A4D) { throw "$Path is not an MZ executable" }
  $reader.BaseStream.Position = 0x3C
  $peOffset = $reader.ReadUInt32()
  $reader.BaseStream.Position = $peOffset
  if ($reader.ReadUInt32() -ne 0x00004550) { throw "$Path has no PE signature" }
  if ($reader.ReadUInt16() -ne 0x8664) { throw "$Path is not an x64 executable" }
  $optionalHeader = $peOffset + 24
  $reader.BaseStream.Position = $optionalHeader
  if ($reader.ReadUInt16() -ne 0x020B) { throw "$Path has no PE32+ optional header" }
  $reader.BaseStream.Position = $optionalHeader + 68
  $subsystem = $reader.ReadUInt16()
  if ($subsystem -ne $ExpectedSubsystem) {
    throw "$Path has subsystem $subsystem; expected $ExpectedSubsystem (2 = Windows GUI, 3 = console)"
  }
  Write-Host "$Path : x64 PE subsystem $subsystem verified"
} finally {
  $reader.Dispose()
}
