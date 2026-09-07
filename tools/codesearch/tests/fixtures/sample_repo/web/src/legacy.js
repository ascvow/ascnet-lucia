/** 旧版轮询实现，保留用于兼容。 */
function pollUntilReady(check, intervalMs) {
  return new Promise((resolve, reject) => {
    const timer = setInterval(() => {
      if (check()) {
        clearInterval(timer);
        resolve(true);
      }
    }, intervalMs);
  });
}

const formatBytes = (bytes) => {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  return `${(bytes / 1024).toFixed(1)} KiB`;
};

module.exports = { pollUntilReady, formatBytes };
