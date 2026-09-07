# Environment for running video-service on the host against the MinIO container.
#
#   docker compose up -d minio minio-init
#   source scripts/local-env.sh
#   cargo run
#
# MinIO stands in for S3 and CloudFront signing is left off (no keypairId), so the
# URLs the API returns point straight at MinIO and are fetchable with curl.
#
# What you can and cannot exercise locally: everything up to and including the
# probe, the faststart remux and the playback fallback works offline — which is most
# of what makes this service interesting. MediaConvert has no local stand-in, so
# renditions themselves need a real roleArn; without one the service still starts,
# still serves, and reports `pending` with reason `not_started`.

export log__level=debug
export server__port=3000

# Metadata on disk rather than Postgres — no second container needed. Note the
# sweep does nothing on this backend (it cannot claim exclusively); set
# videoMetadata__storage=db if you want to watch it run.
export videoMetadata__storage=file
export videoMetadata__parentPath="${PWD}/.local/metadata"
mkdir -p "${videoMetadata__parentPath}"

export s3__endpointUrl=http://localhost:9000
export s3__bucketName=video-service-local
export s3__accessKeyId=minioadmin
export s3__secretAccessKey=minioadmin
export s3__region=us-east-1

export cloudfront__url=http://localhost:9000/video-service-local

if command -v ffmpeg >/dev/null 2>&1; then
  export ffmpeg__bin="$(command -v ffmpeg)"
else
  echo "warning: no ffmpeg found — the faststart remux will fail and uploads that need it will have no playable original" >&2
fi
if command -v ffprobe >/dev/null 2>&1; then
  export ffmpeg__probeBin="$(command -v ffprobe)"
else
  echo "warning: no ffprobe found — every upload will be unprobed and playback will fall back on the file extension" >&2
fi

echo "local env ready: bucket=${s3__bucketName} at ${s3__endpointUrl}, metadata in ${videoMetadata__parentPath}"
