declare module "lucide-react/dist/esm/icons/*.js" {
  import type { ComponentType, SVGProps } from "react";

  const Icon: ComponentType<
    SVGProps<SVGSVGElement> & {
      size?: number | string;
      absoluteStrokeWidth?: boolean;
    }
  >;

  export default Icon;
}
