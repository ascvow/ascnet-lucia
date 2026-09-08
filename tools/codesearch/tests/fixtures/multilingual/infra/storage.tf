// 对象存储保留历史版本，用于误删恢复。
resource "aws_s3_bucket_versioning" "archive" {
  bucket = aws_s3_bucket.archive.id
  versioning_configuration {
    status = "Enabled"
  }
}

// 应用日志保留三十天。
resource "aws_cloudwatch_log_group" "application" {
  name              = "/portal/application"
  retention_in_days = 30
}
