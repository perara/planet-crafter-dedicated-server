// Runs vectors.txt through a real Mono: List<T>.Sort(Comparison<T>) (the demand list) and the struct
// that implements IComparable<T> with List<T>.Sort() (the stations). Prints `name|order`; the
// two must equal vectors.txt's. Built and run with wine-mono (csc.exe Check.cs; wine Check.exe vectors.txt).
using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;

class Item { public int Key; public int Index; }
struct S : IComparable<S>
{
    public int Key; public int Index;
    public int CompareTo(S o) { return o.Key > Key ? -1 : o.Key < Key ? 1 : 0; }
}

class P
{
    static int Main(string[] args)
    {
        int bad = 0;
        foreach (var line in File.ReadAllLines(args[0]))
        {
            var f = line.Split('|');
            var keys = f[1].Split(',').Select(int.Parse).ToArray();
            var items = keys.Select((k, i) => new Item { Key = k, Index = i }).ToList();
            items.Sort((a, b) => a.Key.CompareTo(b.Key));
            var structs = keys.Select((k, i) => new S { Key = k, Index = i }).ToList();
            structs.Sort();
            var a = string.Join(",", items.Select(x => x.Index));
            var b = string.Join(",", structs.Select(x => x.Index));
            if (a != f[2] || b != f[2]) { bad++; Console.WriteLine("DIFF " + f[0] + (a != f[2] ? " comparison" : "") + (b != f[2] ? " struct" : "")); }
        }
        Console.WriteLine(bad == 0 ? "ALL SAME" : bad + " DIFFERENT");
        return bad;
    }
}
