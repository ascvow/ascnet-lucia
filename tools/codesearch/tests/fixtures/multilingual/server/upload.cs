namespace Portal;

/// 上传的文件名不得携带父目录路径。
public static class UploadPolicy {
    public static string SafeName(string supplied) {
        if (supplied.Contains("..")) throw new ArgumentException("Invalid path");
        return Path.GetFileName(supplied);
    }
}
