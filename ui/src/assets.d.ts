/** Stylesheets, imported for the bundler to add to the page. */
declare module "*.css";

/** SVG files imported `with { type: "text" }`: their markup, to draw inline. */
declare module "*.svg" {
  const text: string;
  export default text;
}
