# Independent Windows filesystem evidence: obtain source file extents from
# FSCTL_GET_RETRIEVAL_POINTERS, then read those clusters through the mounted
# archive's volume device. No Own Backup reader supplies this evidence.
Add-Type -Namespace Stand -Name Clusters -MemberDefinition @'
[DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern IntPtr CreateFile(string p,uint a,uint s,IntPtr x,uint c,uint f,IntPtr t);
[DllImport("kernel32.dll", SetLastError=true)] static extern bool DeviceIoControl(IntPtr h,uint c,byte[] i,int n,byte[] o,int m,out int r,IntPtr x);
[DllImport("kernel32.dll", SetLastError=true)] static extern bool ReadFile(IntPtr h,byte[] b,int n,out int r,IntPtr x);
[DllImport("kernel32.dll", SetLastError=true)] static extern bool SetFilePointerEx(IntPtr h,long d,out long p,uint m);
[DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
[DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] public static extern bool GetDiskFreeSpace(string p,out uint spc,out uint bps,out uint f,out uint t);
static IntPtr Open(string p) { var h=CreateFile(p,0x80000000,7,IntPtr.Zero,3,0,IntPtr.Zero);if(h==new IntPtr(-1))throw new System.ComponentModel.Win32Exception();return h; }
public static long[,] Extents(string p) {
 var h=Open(p);try {var output=new byte[1048576];int n;
 if(!DeviceIoControl(h,0x90073,new byte[8],8,output,output.Length,out n,IntPtr.Zero))throw new System.ComponentModel.Win32Exception();
 int count=BitConverter.ToInt32(output,0);if(count<1||16+count*16>n)throw new Exception("bad extent reply");
 long vcn=BitConverter.ToInt64(output,8);var result=new long[count,2];
 for(int k=0;k<count;k++){long next=BitConverter.ToInt64(output,16+k*16);result[k,0]=BitConverter.ToInt64(output,24+k*16);result[k,1]=next-vcn;vcn=next;}return result;
 }finally{CloseHandle(h);}
}
public static bool ZeroAt(string p,long offset,long count) {
 var h=Open(p);try {long position;if(!SetFilePointerEx(h,offset,out position,0))throw new System.ComponentModel.Win32Exception();
 var b=new byte[65536];while(count>0){int want=(int)Math.Min(count,b.Length),got;if(!ReadFile(h,b,want,out got,IntPtr.Zero)||got!=want)throw new System.ComponentModel.Win32Exception();
 for(int k=0;k<got;k++)if(b[k]!=0)return false;count-=got;}return true;
 }finally{CloseHandle(h);}
}
'@
function SourceExtents {
    if(Test-Path "$work\excluded-extents.json"){return}
    [uint32]$spc=0;[uint32]$bps=0;[uint32]$free=0;[uint32]$total=0
    if(-not [Stand.Clusters]::GetDiskFreeSpace('S:\',[ref]$spc,[ref]$bps,[ref]$free,[ref]$total)){throw 'source cluster geometry failed'}
    $cluster=[long]$spc*$bps;$pairs=[Stand.Clusters]::Extents('S:\omit.bin');$extents=@()
    for($i=0;$i -lt $pairs.GetLength(0);$i++){
        $lcn=$pairs[$i,0];$count=$pairs[$i,1]
        Check ($lcn -ge 0 -and $count -gt 0) 'excluded source file has allocated physical clusters'
        Check (-not [Stand.Clusters]::ZeroAt('\\.\S:',$lcn*$cluster,$count*$cluster)) 'excluded source extent contains nonzero data before backup'
        $extents+=@{lcn=$lcn;clusters=$count}
    }
    Report 'excluded-extents' @{cluster_bytes=$cluster;extents=$extents;source_sha256=(Get-FileHash 'S:\omit.bin').Hash;source_data_nonzero=$true}
}
function MountedClusterRequest {
    $request="$work\mounted-clusters.request.json"
    if(-not(Test-Path $request)){return}
    $v=Get-Content $request -Raw -Encoding UTF8|ConvertFrom-Json
    $root=[string]$v.root
    if($root -notmatch '^[A-Z]:\\$' -or $root.Substring(0,1) -in @('C','S','T','R')){throw 'unsafe cluster-check volume'}
    $volume=Get-Volume -DriveLetter $root.Substring(0,1)
    Check ($volume.FileSystemLabel -eq 'ACCEPTSRC') 'mounted cluster probe is on the disposable archive source volume'
    $source=Get-Content "$work\excluded-extents.json" -Raw -Encoding UTF8|ConvertFrom-Json
    $checked=0L
    foreach($extent in $source.extents){
        $bytes=[long]$extent.clusters*$source.cluster_bytes
        Check ([Stand.Clusters]::ZeroAt("\\.\$($root.Substring(0,2))",[long]$extent.lcn*$source.cluster_bytes,$bytes)) 'excluded extent in the mounted OBK contains only zero bytes'
        $checked+=$bytes
    }
    JsonFile "$work\mounted-clusters.result.json" @{id=$v.id;zero=$true;bytes_checked=$checked;root=$root}
    Remove-Item $request -Force
}
