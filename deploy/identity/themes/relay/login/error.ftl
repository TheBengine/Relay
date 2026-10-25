<#import "template.ftl" as layout>
<@layout.registrationLayout displayMessage=false; section>
  <#if section = "header">
    Let's try that again
  <#elseif section = "form">
    <div id="kc-error-message">
      <p class="instruction">${kcSanitize(message.summary)?no_esc}</p>
      <p class="instruction" id="relay-signup-retry-help">If you were creating an account, that Relay name may already be taken. Try a different name.</p>
      <#assign retryUrl=(url.loginRestartFlowUrl)!"/realms/relay-accounts/account/">
      <a id="relay-try-again" class="pf-c-button pf-m-primary pf-m-block" href="${retryUrl}">Try again</a>
      <p class="instruction" style="margin-top: 18px">You can also return to Relay and choose Sign in to start again.</p>
      <#if traceId??>
        <p class="instruction" id="traceId">${msg("traceIdSupportMessage", traceId)}</p>
      </#if>
    </div>
  </#if>
</@layout.registrationLayout>
