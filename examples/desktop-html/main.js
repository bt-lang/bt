const output = document.querySelector("#output");
const stamp = document.querySelector("#stamp");

stamp.addEventListener("click", () => {
  output.value = new Date().toLocaleString();
});
