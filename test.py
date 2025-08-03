import requests

headers = {
    "Host":"ip.sb"
}

url = "http://127.0.0.1:1234"

response = requests.get(url, headers=headers)

print(response.text)