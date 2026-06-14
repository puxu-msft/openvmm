$vm = Get-VM xdp-ws25-1
$vm | Format-List Name,MemoryStartup,MemoryMinimum,MemoryMaximum,DynamicMemoryEnabled,ProcessorCount,AutomaticStartAction,CheckpointType,AutomaticCheckpointsEnabled
