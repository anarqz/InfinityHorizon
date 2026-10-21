// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Diagnostics;

/// <summary>Preserve five local runs; cap current logging at8MiB. Never upload automatically.</summary>
public sealed class BoundedRunLog(string directory)
{
    private readonly object _gate=new();
    private bool _started;
    public void Append(string line)
    {
        lock(_gate)
        {
            try
            {
                Directory.CreateDirectory(directory);var path=Path.Combine(directory,"launcher.log");
                if(!_started)
                {
                    for(var i=5;i>=1;i--)
                    {
                        var from=i==1?path:Path.Combine(directory,$"launcher.previous-{i-1}.log");
                        if(File.Exists(from))File.Move(from,Path.Combine(directory,$"launcher.previous-{i}.log"),true);
                    }
                    _started=true;
                }
                if(File.Exists(path) && new FileInfo(path).Length>=8*1024*1024)return;
                File.AppendAllText(path,$"{DateTimeOffset.Now:yyyy-MM-dd HH:mm:ss.fff zzz} {line[..Math.Min(line.Length,4096)]}{Environment.NewLine}");
            }
            catch(Exception error) when(error is IOException or UnauthorizedAccessException) { }
        }
    }
}
