// Keep password visibility available by click, outside the form's Tab order.
// Fields, validation, autofill and upstream password toggling stay unchanged.
(() => {
    const skipVisibilityButtons = () => {
        document.querySelectorAll("button[data-password-toggle], button[data-password-target]")
            .forEach(button => { button.tabIndex = -1; });
    };
    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", skipVisibilityButtons, { once: true });
    } else {
        skipVisibilityButtons();
    }
})();
