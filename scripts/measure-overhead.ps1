<#
.SYNOPSIS
    Measures what a running SysPilot service costs while idle.

.DESCRIPTION
    Watches the service process of an instance for a while and reports the
    numbers the overhead budget is about: private memory, threads, processor
    time used and context switches (wake-ups) during the window, and the
    size of the executable. Run it with no app open and logging off.

    It starts and stops nothing: start the instance first, for example
    `syspilot-svc --instance SysPilot-Test.dev run` in another terminal.

.EXAMPLE
    ./scripts/measure-overhead.ps1 -Instance SysPilot-Test.dev -Seconds 60
#>
[CmdletBinding()]
param(
    [string] $Instance = 'SysPilot',
    [int] $Seconds = 60
)

$ErrorActionPreference = 'Stop'

# The service process of the instance: the one whose command line names it
# (the product's command line names no instance when run in a console).
$candidates = Get-CimInstance Win32_Process -Filter "Name = 'syspilot-svc.exe'" | Where-Object {
    $_.CommandLine -match "--instance\s+`"?$([regex]::Escape($Instance))(`"|\s|$)" -or
    ($Instance -eq 'SysPilot' -and $_.CommandLine -notmatch '--instance')
} | Where-Object { $_.CommandLine -match '\b(run|service)\b' }
if (-not $candidates) {
    throw "(service): expected a running syspilot-svc for instance $Instance | received: none"
}
$processId = @($candidates)[0].ProcessId

# Context switches so far of each thread of the process, by thread ID, from
# the kernel's counters.
function Get-ContextSwitches {
    $counts = @{}
    Get-CimInstance Win32_PerfRawData_PerfProc_Thread -Filter "IDProcess = $processId" |
        ForEach-Object { $counts[$_.IDThread] = [long] $_.ContextSwitchesPersec }
    $counts
}

$before = Get-Process -Id $processId
$switchesBefore = Get-ContextSwitches
$cpuBefore = $before.TotalProcessorTime
Start-Sleep -Seconds $Seconds
$after = Get-Process -Id $processId
# Per thread, so that a thread that ended during the window does not
# subtract its whole life from the total; a new thread counts from zero.
$switches = 0
foreach ($thread in (Get-ContextSwitches).GetEnumerator()) {
    $switches += $thread.Value - [long] $switchesBefore[$thread.Key]
}

[pscustomobject]@{
    Instance          = $Instance
    Seconds           = $Seconds
    PrivateMB         = [math]::Round($after.PrivateMemorySize64 / 1MB, 2)
    Threads           = $after.Threads.Count
    CpuMilliseconds   = [math]::Round(($after.TotalProcessorTime - $cpuBefore).TotalMilliseconds, 1)
    ContextSwitches   = $switches
    ExecutableMB      = [math]::Round((Get-Item $after.Path).Length / 1MB, 2)
    SensorWorkers     = @(Get-Process syspilot-sensors -ErrorAction SilentlyContinue).Count
}
