const output = document.querySelector("#output");

document.querySelector("#api").onclick = async () => {
  const response = await fetch("/api");
  const data = await response.json();
  output.textContent = JSON.stringify(data, null, 2);
};

document.querySelector("#bridge").onclick = async () => {
  const data = await window.bt.call("hello", { name: "ServerDemo" });
  output.textContent = JSON.stringify(data, null, 2);
};
