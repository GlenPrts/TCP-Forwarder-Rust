import requests
import time
import threading
headers = {
    "Host":"ip.sb"
}

url = "http://127.0.0.1:1234"

n = 200
success_count = 0
fail_count = 0
# 使用多线程进行请求
def send_requests():
    global success_count, fail_count
    for _ in range(n // 10):
        response = requests.get(url, headers=headers)
        if response.status_code == 200:
            success_count += 1
        else:
            fail_count += 1
threads = []
for _ in range(10):
    thread = threading.Thread(target=send_requests)
    threads.append(thread)
    thread.start()
# 等待所有线程完成
for thread in threads:
    thread.join()
# 统计成功和失败的请求

print(f"成功: {success_count}, 失败: {n - success_count}, 总数: {n}")
